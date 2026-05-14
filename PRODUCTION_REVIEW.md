# Production Readiness Review: pg_otel_tracer

**Scope**: Core Rust extension code (`src/*.rs`)<br>
**Severity Legend**: 🔴 Critical | 🟠 High | 🟡 Medium | 🟢 Low / Advisory

---

## 1. Memory Safety & Postgres Memory Contexts

### 🔴 CRITICAL: Hook Allocations Use Rust Heap, Not Postgres Memory Contexts

**Current behavior**: The extension stores all span data in `thread_local!` `RefCell<Vec<RawSpan>>` buffers. When `ExecutorEnd` fires, spans are serialized with `serde_json::to_vec()` (which allocates a `Vec<u8>` on the Rust heap), then pushed to shared memory.

**Why this is correct**: Postgres backends are single-threaded processes. Using Rust heap memory for transient hook data is actually *safer* than using Postgres memory contexts, because:
- Rust's ownership system ensures the `Vec` is dropped when the thread-local goes out of scope
- No risk of leaking into `CurTransactionContext` or `TopMemoryContext`
- The `RefCell` borrow is always released after each hook call

**However**, the `warning!()` macro calls in `ExecutorEnd` may allocate inside Postgres's error-reporting machinery. If the backend is in the middle of a `MemoryContext` switch (e.g., during an error recovery path), these allocations could land in an unexpected context. This is unlikely to cause crashes but could contribute to memory bloat under error-heavy workloads.

**Recommendation**: 🟡 Use `pgrx::error::sub_ctx()` or ensure warning emissions happen before any span buffer mutations that could fail.

### 🟡 MEDIUM: `CStr::from_ptr` on `query_string` / `sourceText`

**Location**: `hooks.rs:118`, `hooks.rs:152`

These strings are owned by Postgres and may not be null-terminated in edge cases (e.g., custom protocol handlers, `COPY` commands, or prepared statements with binary parameters). `CStr::from_ptr` assumes a null terminator. If the string is not null-terminated, this is undefined behavior.

**Mitigation**: Postgres internally null-terminates `query_string` and `sourceText` for all standard query paths. The risk is low but non-zero for exotic protocols.

**Recommendation**: 🟡 Use `libc::strlen` to verify the null terminator exists, or switch to `pg_sys::strlen` bounds-checking.

---

## 2. Crash Safety: Null Pointers & Panics

### 🔴 CRITICAL: Panic in a Postgres Backend Will Kill the Database Process

Rust's `panic!` unwinds the stack. In a Postgres backend process, unwinding across a C FFI boundary is **undefined behavior** that will likely terminate the process (and thus the client connection). Worse, if a panic happens inside `shmem_startup_hook` or the background worker, it can bring down the entire postmaster.

**Audit results**:

| Location | Risk | Analysis |
|----------|------|----------|
| `shared.rs:48` `ShmemInitStruct` | 🟡 Low | Returns null only on OOM; Postgres handles this |
| `shared.rs:66` `(*q).lock.compare_exchange_weak` | 🟢 Very Low | `q` is null-checked before calling `lock_queue` |
| `hooks.rs:115` `CStr::from_ptr(query_string)` | 🟡 Medium | See §1.2 above |
| `hooks.rs:245` `(*query_desc).estate` | 🟢 Very Low | Double-null-checked |
| `exporter.rs:12` `ureq::post(...).send_string()` | 🟠 High | Network I/O in **BGW only** — safe for backend, but BGW crash means no exports |
| `parser.rs:18` `Regex::new(...).unwrap()` | 🟢 Very Low | Happens once at init via `lazy_static`; regex is static |

**🟠 HIGH: `serde_json::to_vec` in `ExecutorEnd` Hook**

```rust
match serde_json::to_vec(&spans) {
    Ok(data) => { ... }
    Err(e) => { warning!(...); }
}
```

`serde_json::to_vec` can panic if the data structure contains invalid types (e.g., non-finite floats, though we use only strings and integers). More importantly, if the span buffer accumulates hundreds of spans, `to_vec` allocates a large contiguous buffer. If allocation fails, Rust's default allocator aborts the process rather than returning an error.

**Recommendation**: 🟠
1. Cap the span buffer size (e.g., flush after 64 spans)
2. Use `serde_json::to_vec` in a `std::panic::catch_unwind` wrapper inside the BGW (already safe because BGW is a separate process)
3. **Never** call `serde_json::to_vec` in a backend hook without a size cap — an OOM here kills the client connection

### 🟡 MEDIUM: `std::ptr::copy_nonoverlapping` in Shared Memory

**Location**: `shared.rs:103`, `shared.rs:138`

`copy_nonoverlapping` is safe as long as the pointers and lengths are valid. The math is:
- `offset = slot * MAX_PAYLOAD_SIZE` where `slot < QUEUE_SIZE`
- `MAX_PAYLOAD_SIZE = 8192`
- Buffer size = `QUEUE_SIZE * MAX_PAYLOAD_SIZE = 1024 * 8192 = 8,388,608`

`slot` is derived from `write_idx % QUEUE_SIZE`. Since `QUEUE_SIZE = 1024`, `slot` is in `[0, 1023]`. `offset` is at most `1023 * 8192 = 8,380,416`. Data length is checked against `MAX_PAYLOAD_SIZE` before push. **The math is correct and safe.**

---

## 3. Process Safety: Shared Memory & Spinlocks

### 🟠 HIGH: Spinlock Busy-Waiting Without Yield

**Location**: `shared.rs:66-75`

```rust
unsafe fn lock_queue(q: *mut SharedQueue) {
    while (*q).lock.compare_exchange_weak(
        false, true, Ordering::Acquire, Ordering::Relaxed
    ).is_err() {
        std::hint::spin_loop();
    }
}
```

On single-CPU systems or under heavy contention, this spinlock can burn CPU cycles. The background worker and backend process may contend for the same lock. `std::hint::spin_loop()` is a PAUSE instruction on x86 — it reduces power consumption but doesn't yield the CPU.

**Risk**: Under very high query rates (1000+ QPS) with a slow BGW, backends could spend noticeable time spinning. On single-core containers (common in CI/test environments), this could cause latency spikes.

**Recommendation**: 🟠 Add a yield after a few spins:

```rust
let mut spins = 0;
while (*q).lock.compare_exchange_weak(...).is_err() {
    std::hint::spin_loop();
    spins += 1;
    if spins > 100 {
        std::thread::yield_now();
        spins = 0;
    }
}
```

### 🟠 HIGH: No Memory Barrier Between Buffer Write and Length Store

**Location**: `shared.rs:103-109`

```rust
std::ptr::copy_nonoverlapping(data.as_ptr(), q.buffer.as_ptr().add(offset) as *mut u8, data.len());
q.lengths[slot] = data.len();
q.write_idx.store(write_idx.wrapping_add(1), Ordering::Relaxed);
```

The buffer write happens before the length store and index update, which is good. But all three operations use `Ordering::Relaxed`. On weakly-ordered architectures (ARM, RISC-V), the compiler or CPU could reorder the buffer write *after* the `write_idx` store, allowing the BGW to read garbage data.

**Analysis**: The spinlock's `Ordering::Release` in `unlock_queue` *should* provide sufficient ordering, but `std::ptr::copy_nonoverlapping` is not an atomic operation and doesn't participate in the C++ memory model's happens-before edges the same way atomic stores do.

**Recommendation**: 🟠 Add a compiler fence before `write_idx.store`:

```rust
std::sync::atomic::fence(Ordering::Release);
q.write_idx.store(write_idx.wrapping_add(1), Ordering::Relaxed);
```

Or use `Ordering::Release` on the `write_idx` store itself.

### 🟡 MEDIUM: `queue_stats` Reads Without Locking

**Location**: `shared.rs:151-163`

```rust
pub fn queue_stats() -> QueueStats {
    unsafe {
        let q = &*queue;
        QueueStats {
            size: q.write_idx.load(Ordering::Relaxed) - q.read_idx.load(Ordering::Relaxed),
            ...
        }
    }
}
```

This is explicitly documented as "best-effort without locking," which is fine for a monitoring function. The subtraction of two atomically-loaded values can produce a stale or even temporarily negative result if the BGW pops between the two loads. Not a correctness issue for monitoring, but the caller should know these are estimates.

---

## 4. Hook Correctness & Chaining

### 🟢 LOW: Hook Chaining Is Correct

All four query hooks properly chain to the previous hook or the standard Postgres function:

```rust
if let Some(prev) = PREV_EXECUTOR_END_HOOK {
    prev(query_desc);
} else {
    pg_sys::standard_ExecutorEnd(query_desc);
}
```

This ensures compatibility with other hook-using extensions like `pg_stat_statements`, `auto_explain`, and `pg_hint_plan`.

### 🟡 MEDIUM: Planner Hook May Not Fire for Cached Plans

**Location**: `hooks.rs:109-140`

The planner hook only fires for queries that go through the planner. Prepared statements and cached plans may bypass the planner on subsequent executions. If the first execution sets `TRACE_CTX`, subsequent executions will reuse it. But if the first execution did NOT have a traceparent (e.g., a prepared statement created without one), subsequent executions with a traceparent won't trigger the planner hook and thus won't set `TRACE_CTX`.

**Analysis**: The `ExecutorStart_hook` has a fallback that parses `sourceText` for traceparent if `TRACE_CTX` is not already set. This handles the cached-plan case. **This is correct design.**

### 🟠 HIGH: ExecutorRun Hook Does Not Handle Re-entrant Calls

Postgres can call `ExecutorRun` multiple times for a single query (e.g., cursors, portals, or subqueries). The current implementation:

```rust
if let Some(ref c) = ctx {
    let mut span = start_span("executor run", c, exec_span_id);
    RUN_SPAN.with(|s| *s.borrow_mut() = Some(span));
}
```

If `ExecutorRun` is called a second time before `ExecutorEnd`, the previous `RUN_SPAN` is overwritten and **lost** (leaked, never finished).

**Recommendation**: 🟠 Track re-entrant calls with a counter or a `Vec` of run spans:

```rust
thread_local! {
    static RUN_SPANS: RefCell<Vec<RawSpan>> = RefCell::new(Vec::new());
}
```

Push on `ExecutorRun`, pop on the matching post-call finish.

---

## 5. Serialization & Queue Overflow

### 🔴 CRITICAL: No Span Buffer Flush Limit in `ExecutorEnd`

**Location**: `hooks.rs:272-295`

```rust
let spans = buf.borrow_mut().drain(..).collect::<Vec<_>>();
match serde_json::to_vec(&spans) {
    Ok(data) => { ... }
}
```

If a single transaction contains thousands of queries (e.g., a `COPY` or a batched INSERT), the span buffer could grow unbounded. `serde_json::to_vec` would try to allocate a multi-megabyte buffer, potentially causing OOM.

**Recommendation**: 🔴 Cap the in-memory buffer and/or flush incrementally:

```rust
const MAX_BUFFERED_SPANS: usize = 64;

// In executor_start_hook or periodically:
SPAN_BUFFER.with(|buf| {
    if buf.borrow().len() >= MAX_BUFFERED_SPANS {
        // Flush early to shared memory
    }
});
```

### 🟠 HIGH: Queue Drop Is Silent (Except Warning)

When the shared-memory queue is full, spans are dropped with a `warning!()`:

```rust
if !queue_push(queue, &data) {
    warning!("pg_otel_tracer: shared memory queue full, spans dropped");
}
```

Under sustained overload, the warning itself is rate-limited by Postgres (to prevent log spam), so you may not even see it. The `queue_stats()` function exposes a `dropped` counter, but there's no automated alerting or metric export.

**Recommendation**: 🟡
1. Add a GUC to configure queue size
2. Add a GUC to configure drop behavior (silent vs. error)
3. Expose dropped counter via `pg_stat_*` or custom view

### 🟡 MEDIUM: JSON Deserialization Failure Drops Entire Batch

**Location**: `bgw.rs:38-48`

```rust
while let Some(data) = queue_pop(queue) {
    match serde_json::from_slice::<Vec<crate::span::RawSpan>>(&data) {
        Ok(spans) => batch.extend(spans),
        Err(e) => { warning!(...); }
    }
}
```

If a single slot in the queue is corrupted (e.g., due to a memory overwrite from another extension), the entire batch for that slot is lost. The BGW doesn't have a recovery mechanism.

**Mitigation**: The spinlock and `copy_nonoverlapping` make corruption unlikely. But if the queue is somehow misaligned (e.g., `ShmemInitStruct` returns a different address on BGW restart), corruption is possible.

**Recommendation**: 🟡 Add a magic number / checksum to queue slots.

---

## 6. Background Worker Reliability

### 🟡 MEDIUM: BGW Crashes Don't Crash Postgres, But Data Is Lost

The background worker is a separate OS process. If it crashes (e.g., `ureq` panics on a malformed URL, or JSON serialization fails), Postgres's postmaster will restart it after the configured restart interval (10 seconds). However:

- Spans in the queue during the crash are **lost** if not drained
- If the crash is in a tight loop (e.g., bad endpoint URL), the BGW will restart repeatedly, consuming CPU

**Recommendation**: 🟡
1. Add a GUC for max BGW restart count before giving up
2. Validate the endpoint URL at startup
3. Wrap the export loop in `catch_unwind` to prevent panics from killing the BGW

### 🟡 MEDIUM: No Batched Export on Shutdown

When Postgres shuts down, the BGW receives `SIGTERM` and exits the loop:

```rust
while BackgroundWorker::wait_latch(Some(Duration::from_millis(500))) {
    if BackgroundWorker::sigterm_received() { break; }
}
```

Any spans still in the queue are **not** drained before exit. For a graceful shutdown, the BGW should do one final drain-and-export after the loop.

**Recommendation**: 🟡 Add a post-loop drain:

```rust
// After the main loop exits:
drain_and_export(&mut batch);
```

### 🟢 LOW: `ureq` Has No Timeout Configured

**Location**: `exporter.rs:12`

```rust
let response = ureq::post(&endpoint)
    .set("Content-Type", "application/json")
    .send_string(&payload)?;
```

`ureq` uses a default timeout of 30 seconds. If the collector is unreachable, the BGW blocks for 30s before retrying. With a 500ms polling loop, this means ~60 queue drain attempts are blocked.

**Mitigation**: The queue continues to accept pushes while the BGW is blocked (it's a separate process). The risk is BGW lag, not data loss.

**Recommendation**: 🟢 Explicitly set a shorter timeout:

```rust
ureq::post(&endpoint)
    .timeout(Duration::from_secs(5))
    .send_string(&payload)?;
```

---

## 7. Performance

### 🟡 MEDIUM: Per-Query Regex Compilation

The `TRACEPARENT_RE` is wrapped in `lazy_static!`, so the regex is compiled once. **Good.**

### 🟡 MEDIUM: Per-Query JSON Serialization

`ExecutorEnd` serializes all buffered spans to JSON. For a transaction with 100 queries, this is a single `serde_json::to_vec` call on ~100 spans. The payload size is roughly 1-2KB per span, so 100-200KB per batch. This is acceptable for most workloads but could be a bottleneck for very high-QPS systems.

**Recommendation**: 🟡 Consider a binary serialization format (bincode, MessagePack) for shared memory, with JSON conversion only in the BGW.

### 🟢 LOW: Spinlock Contention

With a 500ms BGW polling interval and 1024 queue slots, the queue can absorb ~2000 span batches per second before dropping. For typical OLTP workloads (10-100 spans per second), contention is negligible.

---

## 8. Security

### 🟡 MEDIUM: No Authentication on OTLP Export

The `ureq` client sends spans to `OTEL_EXPORTER_OTLP_ENDPOINT` without authentication. If the endpoint is exposed to the internet (e.g., a misconfigured collector), trace data leaks.

**Recommendation**: 🟡 Document that the endpoint should be on a private network. Future versions could support OTLP headers (API keys) via GUC.

### 🟢 LOW: SQL Comments Are Semantically Transparent

The traceparent regex only reads SQL comments. It never modifies the query text or injects new SQL. The extension is read-only with respect to query execution.

---

## 9. Operational Concerns

### 🔴 CRITICAL: No Enable/Disable Toggle

There is no GUC to disable the extension at runtime. If the extension causes issues, the only remedy is to remove it from `shared_preload_libraries` and restart Postgres.

**Recommendation**: 🔴 Add `pg_otel_tracer.enabled` GUC that bypasses all hooks when false.

### 🟠 HIGH: No Endpoint Configuration Without Restart

The OTLP endpoint is read from `OTEL_EXPORTER_OTLP_ENDPOINT` env var at BGW startup. Changing it requires a Postgres restart.

**Recommendation**: 🟠 Add `pg_otel_tracer.endpoint` GUC that the BGW reads dynamically.

### 🟡 MEDIUM: No Sampling Rate Control

Every query with a traceparent comment is traced. In high-throughput systems, this could export 100% of queries, overwhelming the collector.

**Recommendation**: 🟡 Add `pg_otel_tracer.sample_rate` GUC (0.0-1.0) that drops spans probabilistically.

---

## Summary: Is It Production-Ready?

### ✅ What Is Solid

| Aspect | Rating | Notes |
|--------|--------|-------|
| Hook chaining | ✅ Correct | Always calls prev hook or standard function |
| Memory isolation | ✅ Good | Rust heap for transient data, no context leaks |
| Thread-local state | ✅ Correct | Single-threaded backends, no shared state |
| Spinlock math | ✅ Safe | Bounds-checked offsets, correct buffer sizing |
| ShmemInitStruct | ✅ Correct | PG15+ compatible shmem_request_hook |
| Parser | ✅ Robust | Static regex, handles quotes and missing traceparent |
| BGW isolation | ✅ Good | Separate process, queue-based decoupling |

### ⚠️ What Needs Work Before Production

| Issue | Severity | Fix Complexity |
|-------|----------|---------------|
| No enable/disable GUC | 🔴 Critical | Low |
| No span buffer size cap | 🔴 Critical | Low |
| No `catch_unwind` in BGW export | 🟠 High | Low |
| Spinlock lacks yield on contention | 🟠 High | Low |
| Missing memory fence on queue write | 🟠 High | Low |
| No graceful shutdown drain | 🟡 Medium | Low |
| ExecutorRun re-entrancy leak | 🟠 High | Medium |
| No sampling rate control | 🟡 Medium | Low |
| No endpoint GUC | 🟠 High | Low |

### 🎯 Recommendation

**Current state**: Suitable for **staging, development, and low-throughput production** with monitoring.

**Before high-throughput production**:
1. Implement the 🔴 and 🟠 fixes above
2. Add a load test: 1000+ QPS for 1 hour, verify no memory growth or backend crashes
3. Run `pg_regress` or pgrx tests with `pg_stat_statements` and `auto_explain` simultaneously to verify hook compatibility
4. Test BGW crash recovery: kill -9 the BGW process, verify postmaster restarts it
5. Test queue overflow behavior under sustained overload

The architecture is sound. The code quality is high for a first implementation. With the identified hardening, this can be a reliable production extension.
