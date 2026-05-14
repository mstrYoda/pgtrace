use pgrx::prelude::*;

pgrx::pg_module_magic!();

mod bgw;
mod exporter;
mod hooks;
mod parser;
mod shared;
mod span;
mod wait_events;

/// Extension entry point. Called once per backend when the shared library is loaded.
/// Because we use shared memory and background workers, the extension must be listed
/// in `shared_preload_libraries` in postgresql.conf.
#[pg_guard]
pub extern "C" fn _PG_init() {
    // PG15+ requires shared-memory requests to happen inside shmem_request_hook.
    // During initdb (bootstrap) there is no postmaster, so the hook is never
    // triggered and we avoid the fatal error.
    unsafe { hooks::install_shmem_request_hook() };

    // Install shared-memory startup hook (allocates queue after shmem is created).
    unsafe { hooks::install_shmem_hook() };

    // Install query lifecycle hooks.
    unsafe { hooks::install_query_hooks() };

    // Register the background worker that drains spans and exports them.
    bgw::register_worker();

    log!("pg_otel_tracer: extension initialized");
}

/// SQL-callable function returning the extension version.
#[pg_extern]
fn pg_otel_tracer_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// SQL-callable table function returning runtime status metrics.
#[pg_extern]
fn pg_otel_tracer_status() -> TableIterator<'static, (name!(metric, String), name!(value, String))> {
    let stats = shared::queue_stats();
    TableIterator::new(vec![
        ("version".into(), env!("CARGO_PKG_VERSION").into()),
        ("queue_size".into(), stats.size.to_string()),
        ("queue_dropped".into(), stats.dropped.to_string()),
    ])
}

// ─────────────────────────────────────────────────────────────
// pgrx test scaffolding
// ─────────────────────────────────────────────────────────────

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use super::*;

    #[pg_test]
    fn test_version() {
        assert_eq!(pg_otel_tracer_version(), "0.1.0");
    }

    #[pg_test]
    fn test_parse_traceparent() {
        let sql = "SELECT 1 /*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/";
        let ctx = parser::extract_traceparent(sql);
        assert!(ctx.is_some());
        let ctx = ctx.unwrap();
        assert_eq!(ctx.trace_id, "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(ctx.parent_span_id, "b7ad6b7169203331");
    }
}

/// Required by pgrx for test integration.
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["shared_preload_libraries = 'pg_otel_tracer'"]
    }
}
