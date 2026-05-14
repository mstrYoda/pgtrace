# OpenTelemetry Tracing Extension for PostgreSQL

## Overview

`pg_otel_tracer` is a PostgreSQL extension written in Rust (via **pgrx**) that bridges the observability gap between backend APIs and Postgres by extracting W3C `traceparent` IDs from SQL comments (sqlcommenter) and exporting OpenTelemetry spans for query lifecycle events.

## Architecture

```
┌─────────────────┐      sqlcommenter       ┌─────────────────┐
│  Backend API    │ ── SELECT ... /*tp=...*/──>│   PostgreSQL    │
│  (traceparent)  │                           │                 │
└─────────────────┘                           │  ┌───────────┐  │
                                              │  │  Hooks    │  │
                                              │  │(Planner  │  │
                                              │  │ Executor) │  │
                                              │  └─────┬─────┘  │
                                              │        │         │
                                              │  ┌─────▼─────┐   │
                                              │  │ Shared    │   │
                                              │  │ Memory    │   │
                                              │  │ Ring Buf  │   │
                                              │  └─────┬─────┘   │
                                              │        │         │
                                              │  ┌─────▼─────┐   │
                                              │  │ Background│   │
                                              │  │ Worker    │   │
                                              │  │ (OTLP)    │   │
                                              │  └─────┬─────┘   │
                                              └────────┼─────────┘
                                                       │
                                                       ▼
                                              ┌─────────────────┐
                                              │  OTEL Collector │
                                              │ (Jaeger/Tempo)  │
                                              └─────────────────┘
```

## Features

1. **Trace Context Extraction**: Parses `traceparent` from SQL comments (sqlcommenter format).
2. **Query Lifecycle Spans**:
   - `planner` — query planning/optimization
   - `query execution` — overall execution
   - `executor run` — data retrieval
3. **Wait Event Sampling**: Captures Postgres lock and I/O wait events during query execution.
4. **Asynchronous Export**: Background worker drains spans from shared memory and exports via OTLP/HTTP JSON.
5. **Ring Buffer**: Shared-memory MPSC queue prevents blocking the backend.

## Build Requirements

- Rust 1.70+ (tested on 1.95.0)
- `cargo-pgrx` (`cargo install cargo-pgrx`)
- PostgreSQL development headers (13–16)
- For Linux cross-compilation from macOS: a Linux linker (e.g., `x86_64-linux-musl-gcc`)

## Build & Install

### 1. Install pgrx

```bash
cargo install cargo-pgrx --locked
cargo pgrx init   # downloads & compiles supported Postgres versions
```

### 2. Build the extension

```bash
cargo pgrx run pg16   # quick development iteration
# or
cargo pgrx package    # produce installable artifacts
```

### 3. Install in PostgreSQL

Copy the `.so` and SQL files from `target/release/pg_otel_tracer-pg16/` (or the package directory) into your Postgres `lib` and `share/extension` directories, then:

```sql
CREATE EXTENSION pg_otel_tracer;
```

> **Critical**: Because this extension uses shared memory and a background worker, it **must** be preloaded:
>
> ```conf
> # postgresql.conf
> shared_preload_libraries = 'pg_otel_tracer'
> ```
>
> Restart PostgreSQL after changing this.

## Configuration

Currently the OTLP endpoint is read from the environment variable `OTEL_EXPORTER_OTLP_ENDPOINT` (default: `http://localhost:4318/v1/traces`).

Future versions will expose GUC variables:
- `pg_otel_tracer.endpoint`
- `pg_otel_tracer.enabled`

## Cross-Compilation (macOS → Linux)

Since PostgreSQL servers typically run on Linux, you may need to cross-compile:

```bash
# Install a Linux cross-compiler toolchain, e.g.:
brew install FiloSottile/musl-cross/musl-cross

# Edit .cargo/config.toml (example provided) to set the linker, then:
cargo build --release --target x86_64-unknown-linux-gnu
```

## Testing

```bash
cargo pgrx test pg16
```

## Implementation Notes

### Memory Contexts
All hook allocations that outlive the hook call are kept in thread-local `RefCell` buffers and flushed in `ExecutorEnd`. No persistent allocations are made in `CurTransactionContext` or other Postgres memory contexts, preventing leaks.

### Thread Safety
- Each backend process is single-threaded; we use `thread_local!` for per-backend state.
- The shared-memory ring buffer is protected by a Postgres `LWLock` (lightweight lock) for MPSC safety.
- The background worker is a separate OS process; it reads from the same shared-memory queue.

### Wait Events
We sample `pg_sys::MyProc->wait_event_info` before and after `ExecutorRun`. Brief waits may be missed; future work could add a lightweight timer-based sampler or a dedicated monitoring thread.

## Project Structure

| File | Purpose |
|------|---------|
| `src/lib.rs` | Extension entry point (`_PG_init`), GUCs, SQL functions |
| `src/hooks.rs` | Planner, ExecutorStart, ExecutorRun, ExecutorEnd hooks |
| `src/parser.rs` | Regex-based `traceparent` extraction from SQL comments |
| `src/span.rs` | `RawSpan` / `RawEvent` types and ID generation |
| `src/shared.rs` | Shared-memory ring buffer + LWLock |
| `src/bgw.rs` | Background worker (drain queue → export) |
| `src/exporter.rs` | OTLP/HTTP JSON payload builder + `ureq` client |
| `src/wait_events.rs` | `MyProc->wait_event_info` decoder |
