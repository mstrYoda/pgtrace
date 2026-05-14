# pg_otel_tracer

OpenTelemetry tracing extension for PostgreSQL. Extracts W3C `traceparent` from SQL comments and exports query lifecycle spans via OTLP/HTTP.

```
Backend API          PostgreSQL                  OTEL Collector
     │    SQL + traceparent    │                       │
     │ ──────────────────────> │  planner span         │
     │                         │  query execution span │
     │                         │  executor run span    │
     │                         │ ────────┬───────────> │
     │                         │  Shared │ Memory      │
     │                         │  Queue  │             │
     │                         │ ────────┘             │
```

## What It Does

1. Your app injects `traceparent` into SQL comments (sqlcommenter)
2. The extension intercepts query hooks (`planner`, `ExecutorStart`, `ExecutorRun`, `ExecutorEnd`)
3. Spans are batched in shared memory and exported asynchronously by a background worker
4. You see the full trace in Jaeger/Tempo — from HTTP handler down to Postgres internals

## Quick Start (Docker)

```bash
git clone https://github.com/mstrYoda/pg_otel_tracer.git
cd pg_otel_tracer
docker compose up --build
```

This starts Postgres 16 + OTEL Collector + Jaeger + Go demo app.

```bash
# Send a traced request
curl -X POST http://localhost:8080/users \
  -d '{"name":"Alice","email":"alice@example.com"}'

# View traces
open http://localhost:16686
```

> First build compiles the Rust extension inside the Postgres image (5–15 min).

## Manual Installation

### Prerequisites

- Rust 1.70+
- `cargo install cargo-pgrx --version 0.11.2 --locked`
- `cargo pgrx init`
- PostgreSQL 13–16 dev headers

### Build & Install

```bash
cargo pgrx package --pg-config $(which pg_config)

# Copy artifacts to Postgres dirs
PG_CONFIG=$(which pg_config)
cp target/release/pg_otel_tracer-pg16/usr/share/postgresql/16/extension/* \
   "$($PG_CONFIG --sharedir)/extension/"
cp target/release/pg_otel_tracer-pg16/usr/lib/postgresql/16/lib/* \
   "$($PG_CONFIG --pkglibdir)/"
```

### Configure PostgreSQL

Add to `postgresql.conf`:

```conf
shared_preload_libraries = 'pg_otel_tracer'
```

Restart Postgres, then:

```sql
CREATE EXTENSION pg_otel_tracer;
```

## Configuration

| Setting | How to Set | Default | Description |
|---------|-----------|---------|-------------|
| **Enable/disable** | `SELECT pg_otel_tracer_set_enabled(false)` | `true` | Emergency off switch — no restart needed |
| **Sampling rate** | `SELECT pg_otel_tracer_set_sample_rate(0.1)` | `1.0` | Fraction of queries to trace (0.0–1.0) |
| **OTLP endpoint** | `OTEL_EXPORTER_OTLP_ENDPOINT` env var | `http://localhost:4318/v1/traces` | Where spans are sent |
| **Boot-time enable** | `PG_OTEL_TRACER_ENABLED` env var | `true` | Initial state on server startup |
| **Boot-time sample rate** | `PG_OTEL_TRACER_SAMPLE_RATE` env var | `1.0` | Initial sampling rate |

### Production Tuning Example

```sql
-- Start with 1% sampling to measure overhead
SELECT pg_otel_tracer_set_sample_rate(0.01);

-- Monitor queue health
SELECT * FROM pg_otel_tracer_status();
--  metric       | value
-- --------------+--------
--  version      | 0.1.0
--  enabled      | true
--  sample_rate  | 0.0100
--  queue_size   | 3
--  queue_dropped| 0

-- If queue_dropped > 0, the collector can't keep up — reduce sample_rate
-- or scale the collector. If problems persist, disable immediately:
SELECT pg_otel_tracer_set_enabled(false);
```

## Architecture

The extension uses four Postgres hooks to trace query lifecycle:

| Hook | Span | What It Measures |
|------|------|-----------------|
| `planner_hook` | `planner` | Query optimization |
| `ExecutorStart_hook` | `query execution` | Execution start |
| `ExecutorRun_hook` | `executor run` | Data retrieval + wait events |
| `ExecutorEnd_hook` | — | Finalizes spans + flushes to queue |

**Thread-local buffers** store spans per backend. **Shared-memory ring buffer** (1024 slots, 8KB each) passes data to the background worker. The **BGW** exports via OTLP/HTTP JSON every 500ms.

Key design decisions:
- **Drop on overflow** — never block the database for telemetry
- **Bounded buffer** — max 64 spans per backend before forced flush
- **Spinlock + yield** — protects the queue without LWLock ABI fragility
- **catch_unwind** — panics in the BGW are trapped, not propagated
- **Exponential backoff retry** — 3 attempts on export failure

## How Your App Injects Traceparent

The extension reads W3C `traceparent` from SQL comments:

```sql
SELECT * FROM users
/*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/;
```

The included Go demo does this automatically. For other languages, append the trace context as a trailing comment before sending the query to Postgres.

## Project Structure

```
src/
  lib.rs       # _PG_init, SQL functions
  hooks.rs     # Planner + Executor hooks
  parser.rs    # Traceparent regex extraction
  span.rs      # RawSpan types + ID generation
  shared.rs    # Shmem ring buffer + spinlock
  bgw.rs       # Background worker (drain → export)
  exporter.rs  # OTLP/HTTP JSON payload
  wait_events.rs # MyProc wait event sampling
  config.rs    # Runtime enable/sampling toggles
demo-go/       # End-to-end Go/GORM demo
```

## Cross-Compilation

```bash
# macOS → Linux
brew install FiloSottile/musl-cross/musl-cross
# Edit .cargo/config.toml to set linker
cargo build --release --target x86_64-unknown-linux-gnu
```

## Testing

```bash
# Unit tests
cargo pgrx test pg16

# Local Postgres instance
cargo pgrx run pg16
```

## Stopping

```bash
docker compose down        # stop
docker compose down -v     # stop + wipe data
```

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `shared_preload_libraries` error | Extension not preloaded | Add to `postgresql.conf`, restart |
| No spans in Jaeger | Collector unreachable | Check `OTEL_EXPORTER_OTLP_ENDPOINT`, verify collector health |
| `queue_dropped` increasing | Collector can't keep up | Reduce `sample_rate` or scale collector |
| High CPU | BGW restart loop | Check collector endpoint, verify network |

## Contributing

Areas for contribution:
- GUC variables (`pg_otel_tracer.enabled` as native GUC)
- gRPC OTLP exporter
- pg_stat_statements integration
- Support for Postgres 13–15

## License

Apache-2.0
