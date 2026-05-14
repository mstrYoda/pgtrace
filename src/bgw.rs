use pgrx::prelude::*;
use pgrx::bgworkers::{BackgroundWorker, BackgroundWorkerBuilder, SignalWakeFlags};
use std::time::Duration;

use crate::exporter::export_batch;
use crate::shared::{get_queue, queue_pop};

/// Register the background worker with the postmaster.
pub fn register_worker() {
    BackgroundWorkerBuilder::new("pg_otel_tracer_worker")
        .set_library("pg_otel_tracer")
        .set_function("pg_otel_worker_main")
        .set_argument(0i32.into_datum())
        .set_restart_time(Some(Duration::from_secs(10)))
        .enable_spi_access()
        .load();
}

/// Background worker entry point.  Runs in a separate OS process.
#[pg_guard]
#[no_mangle]
pub extern "C" fn pg_otel_worker_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
    BackgroundWorker::connect_worker_to_spi(Some("postgres"), None);

    log!("pg_otel_tracer: background worker started");

    let mut batch: Vec<crate::span::RawSpan> = Vec::new();

    while BackgroundWorker::wait_latch(Some(Duration::from_millis(500))) {
        if BackgroundWorker::sigterm_received() {
            break;
        }

        // Drain the shared-memory queue.
        unsafe {
            let queue = get_queue();
            while let Some(data) = queue_pop(queue) {
                match serde_json::from_slice::<Vec<crate::span::RawSpan>>(&data) {
                    Ok(spans) => batch.extend(spans),
                    Err(e) => {
                        warning!(
                            "pg_otel_tracer: failed to deserialize spans: {}",
                            e
                        );
                    }
                }
            }
        }

        if !batch.is_empty() {
            if let Err(e) = export_batch(&batch) {
                warning!("pg_otel_tracer: export failed: {}", e);
            } else {
                log!("pg_otel_tracer: exported {} spans", batch.len());
            }
            batch.clear();
        }
    }

    log!("pg_otel_tracer: background worker shutting down");
}
