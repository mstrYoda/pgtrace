# pg_otel_tracer

<p align="center">
  <strong>OpenTelemetry Tracing Extension for PostgreSQL</strong><br>
  Bridge the observability gap between your backend APIs and Postgres.
</p>

<p align="center">
  <a href="#features">Features</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#docker-full-stack">Docker Stack</a> •
  <a href="#configuration">Configuration</a> •
  <a href="#how-it-works">How It Works</a>
</p>

---

## Overview

`pg_otel_tracer` is a PostgreSQL extension written in **Rust** (via [pgrx](https://github.com/pgcentralfoundation/pgrx)) that extracts W3C `traceparent` IDs from SQL comments ([sqlcommenter](https://google.github.io/sqlcommenter/) format) and exports OpenTelemetry spans for query lifecycle events — without blocking the database backend.

If your backend API is already instrumented with OpenTelemetry, this extension lets you **see inside Postgres** as part of the same distributed trace.

```
┌─────────────────┐      sqlcommenter          ┌─────────────────┐
│  Backend API    │ ── SELECT ... /*tp=...*/──>│   PostgreSQL    │
│  (traceparent)  │                            │  ┌───────────┐  │
└─────────────────┘                            │  │  Hooks    │  │
                                               │  │(Planner   │  │
                                               │  │ Executor) │  │
                                               │  └─────┬─────┘  │
                                               │        │        │
                                               │  ┌─────▼─────┐  │
                                               │  │ Shared    │  │
                                               │  │ Memory    │  │
                                               │  │ Ring Buf  │  │
                                               │  └─────┬─────┘  │
                                               │        │        │
                                               │  ┌─────▼─────┐  │
                                               │  │ Background│  │
                                               │  │ Worker    │  │
                                               │  │ (OTLP)    │  │
                                               │  └─────┬─────┘  │
                                               └────────┼────────┘
                                                        │
                                                        ▼
                                                ┌─────────────────┐
                                                │  OTEL Collector │
                                                │ (Jaeger/Tempo)  │
                                                └─────────────────┘
```

---

## Features

- **Trace Context Extraction** — Parses W3C `traceparent` from sqlcommenter-style SQL comments.
- **Query Lifecycle Spans** — Emits spans for `planner`, `query execution`, and `executor run`.
- **Wait Event Sampling** — Captures Postgres lock and I/O wait events during query execution.
- **Asynchronous Export** — Background worker drains spans from shared memory and exports via OTLP/HTTP JSON.
- **Non-Blocking** — Shared-memory ring buffer ensures hooks never wait on I/O.
- **Zero Configuration** — Works out of the box with any OTLP-compatible collector.

---

## Architecture

### Hook Points

The extension intercepts four Postgres hooks to trace the full query lifecycle:

| Hook | Span Created | What It Measures |
|------|-------------|------------------|
| `planner_hook` | `planner` | Query planning & optimization |
| `ExecutorStart_hook` | `query execution` | Overall execution start |
| `ExecutorRun_hook` | `executor run` | Data retrieval / scan |
| `ExecutorEnd_hook` | — | Finalizes spans, flushes to queue |

### Per-Span Data

- **Timing**: `startTimeUnixNano` / `endTimeUnixNano` for every phase
- **Row Count**: `db.row_count` from `estate->es_processed`
- **Wait Events**: `wait_event_before` and `wait_event_after` events sampling `MyProc->wait_event_info`
- **Parent ID**: Extracted from `/*traceparent='00-<trace_id>-<parent_id>-<flags>'*/`

### Background Worker (BGW)

A separate OS process reads batches from the shared-memory ring buffer and exports them via OTLP/HTTP JSON to your collector. The backend process never performs I/O.

---

## Quick Start

### Prerequisites

- Docker & Docker Compose
- (Optional) Rust 1.70+ if building locally

### Docker Full Stack

The fastest way to see everything working end-to-end is using the provided Docker Compose stack, which includes:

- **PostgreSQL 16** with `pg_otel_tracer` preloaded
- **OpenTelemetry Collector**
- **Jaeger** (trace viewer UI)
- **Go Demo App** (GORM + traceparent injection)

```bash
# Clone and start everything
cd /path/to/postgres-extension
docker compose up --build
```

> **Note:** The first build compiles the Rust extension inside the Postgres image. This takes 5–15 minutes. Grab coffee.

Once running:

```bash
# Create a user via the Go API
curl -X POST http://localhost:8080/users \
  -H "Content-Type: application/json" \
  -d '{"name":"Alice","email":"alice@example.com"}'

# List users
curl http://localhost:8080/users
```

Open [http://localhost:16686](http://localhost:16686) in Jaeger and search for the `demo-go` service. You will see the full distributed trace including the Postgres `planner`, `query execution`, and `executor run` spans.

### Stopping

```bash
docker compose down        # stop
docker compose down -v     # stop + wipe data
```

---

## Manual Installation

If you prefer to install the extension into an existing PostgreSQL instance:

### 1. Build

```bash
# Install pgrx
cargo install cargo-pgrx --version 0.11.2 --locked
cargo pgrx init

# Build
cargo pgrx package --pg-config $(which pg_config)
```

### 2. Install

Copy the artifacts to your Postgres directories:

```bash
PG_CONFIG=$(which pg_config)
SHARE_DIR=$($PG_CONFIG --sharedir)
LIB_DIR=$($PG_CONFIG --pkglibdir)

cp target/release/pg_otel_tracer-pg16/usr/share/postgresql/16/extension/* "$SHARE_DIR/extension/"
cp target/release/pg_otel_tracer-pg16/usr/lib/postgresql/16/lib/* "$LIB_DIR/"
```

### 3. Configure

Add to `postgresql.conf`:

```conf
shared_preload_libraries = 'pg_otel_tracer'
```

Restart PostgreSQL.

### 4. Create Extension

```sql
CREATE EXTENSION pg_otel_tracer;
```

Verify:

```sql
SELECT * FROM pg_otel_tracer_status();
--  metric      | value
-- -------------+--------
--  version     | 0.1.0
--  queue_size  | 0
--  queue_dropped| 0
```

---

## Configuration

The extension reads the OTLP endpoint from an environment variable:

| Variable | Default | Description |
|----------|---------|-------------|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `http://localhost:4318/v1/traces` | OTLP/HTTP traces endpoint |

In Docker Compose, set it on the Postgres service:

```yaml
services:
  postgres:
    environment:
      OTEL_EXPORTER_OTLP_ENDPOINT: http://otel-collector:4318/v1/traces
```

---

## How It Works

### 1. Your App Injects Trace Context

Any backend that appends W3C `traceparent` to SQL comments will work. Example raw SQL:

```sql
SELECT id, name FROM users
/*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/;
```

The included Go demo does this automatically using a custom `sql.DB` wrapper:

```go
func injectTraceparent(ctx context.Context, query string) string {
    sc := trace.SpanFromContext(ctx).SpanContext()
    tp := fmt.Sprintf("00-%s-%s-%02x", sc.TraceID(), sc.SpanID(), sc.TraceFlags())
    return query + fmt.Sprintf(" /*traceparent='%s'*/", tp)
}
```

### 2. Extension Extracts & Traces

- The `planner_hook` regex-parses the `traceparent` from `query_string`.
- A `planner` span starts, measuring planning time.
- `ExecutorStart_hook` starts the `query execution` span.
- `ExecutorRun_hook` starts the `executor run` span and samples wait events.
- `ExecutorEnd_hook` finalizes all spans, serializes them, and pushes to the shared-memory queue.

### 3. Background Worker Exports

Every 500ms, the BGW:

1. Drains the shared-memory ring buffer (MPSC, spinlock-protected)
2. Deserializes span batches
3. Builds an OTLP/HTTP JSON payload
4. POSTs it to the configured collector

### 4. View in Jaeger

Search for the `postgresql` service. Each traced query produces a nested span tree:

```
demo-go: POST /users
└── postgresql: planner
    └── postgresql: query execution
        └── postgresql: executor run
            ├── event: wait_event_before  {type: "Lock", event: "0x00000701"}
            ├── attribute: db.row_count = "1"
            └── event: wait_event_after   {type: "None", event: "0x00000000"}
```

---

## Project Structure

```
.
├── src/
│   ├── lib.rs          # Extension entry point (_PG_init), GUCs
│   ├── hooks.rs        # Planner + ExecutorStart/Run/End hooks
│   ├── parser.rs       # Regex traceparent extractor
│   ├── span.rs         # RawSpan / RawEvent types + ID generation
│   ├── shared.rs       # Shared-memory ring buffer + spinlock
│   ├── bgw.rs          # Background worker (drain → export)
│   ├── exporter.rs     # OTLP/HTTP JSON payload + ureq client
│   └── wait_events.rs  # MyProc->wait_event_info decoder
├── demo-go/            # End-to-end Go/GORM demo app
├── Dockerfile.pg_otel  # Postgres image build
├── docker-compose.yml  # Full stack (Postgres + Collector + Jaeger + Go)
├── otel-collector-config.yaml
└── Cargo.toml
```

---

## Local Development

```bash
# Run tests
cargo pgrx test pg16

# Run inside a local Postgres instance
cargo pgrx run pg16

# In psql:
CREATE EXTENSION pg_otel_tracer;
SELECT 1 /*traceparent='00-11111111111111111111111111111111-2222222222222222-01'*/;
```

---

## Cross-Compilation

To build the `.so` for Linux from macOS:

```bash
# Install a Linux cross-compiler, e.g.:
brew install FiloSottile/musl-cross/musl-cross

# Edit .cargo/config.toml (example provided) to set the linker, then:
cargo build --release --target x86_64-unknown-linux-gnu
```

---

## Known Limitations

- **Wait event sampling is point-in-time**: Brief waits between the before/after samples may be missed. A timer-based sampler could improve coverage.
- **No query text export**: SQL statements are intentionally not attached to spans to avoid leaking PII. This can be toggled behind a GUC in future versions.
- **OTLP/HTTP only**: gRPC export is not yet implemented.

---

## Contributing

Contributions are welcome! Areas we'd love help with:

- GUC variables for endpoint / toggle features
- gRPC OTLP exporter
- Timer-based wait event sampler
- pg_stat_statements integration
- Support for more Postgres versions (13–15)

Please open an issue or PR.

---

## License

Apache-2.0

---

<p align="center">
  Built with <a href="https://github.com/pgcentralfoundation/pgrx">pgrx</a> and Rust.
</p>
