use pgrx::prelude::*;
use pgrx::bgworkers::{BackgroundWorker, BackgroundWorkerBuilder, SignalWakeFlags};
use std::panic;
use std::time::Duration;

use crate::config;
use crate::exporter::export_batch;
use crate::shared::{get_queue, queue_pop};

/// Maximum spans to export in a single HTTP request.
const EXPORT_CHUNK_SIZE: usize = 100;

/// Register the background worker with the postmaster.
pub fn register_worker() {
    BackgroundWorkerBuilder::new("pg_otel_tracer_worker")
        .set_library("pg_otel_tracer")
        .set_function("pg_otel_worker_main")
        .set_argument(0i32.into_datum())
        .set_restart_time(Some(Duration::from_secs(10)))
        .set_flags(pg_sys::BGWORKER_SHMEM_ACCESS as i32)
        .load();
}

/// Background worker entry point.  Runs in a separate OS process.
#[pg_guard]
#[no_mangle]
pub extern "C" fn pg_otel_worker_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);

    log!("pg_otel_tracer: background worker started");

    let mut batch: Vec<crate::span::RawSpan> = Vec::new();

    while BackgroundWorker::wait_latch(Some(Duration::from_millis(500))) {
        if BackgroundWorker::sigterm_received() {
            break;
        }

        // Reload configuration on SIGHUP (e.g. after ALTER SYSTEM + reload).
        if BackgroundWorker::sighup_received() {
            log!("pg_otel_tracer: SIGHUP received, reloading config");
            // In future versions this will re-read GUCs.
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

        // Export in fixed-size chunks so we never build a giant HTTP request.
        while !batch.is_empty() {
            let chunk_size = batch.len().min(EXPORT_CHUNK_SIZE);
            let chunk: Vec<_> = batch.drain(..chunk_size).collect();

            // catch_unwind prevents any panic (ureq, serde, etc.) from killing
            // the background worker process.
            let result = panic::catch_unwind(|| {
                export_batch_with_retry(&chunk)
            });

            match result {
                Ok(Ok(())) => {
                    log!("pg_otel_tracer: exported {} spans", chunk.len());
                }
                Ok(Err(e)) => {
                    warning!("pg_otel_tracer: export failed: {}", e);
                }
                Err(_) => {
                    warning!("pg_otel_tracer: export panicked, will retry next cycle");
                }
            }
        }
    }

    // Graceful shutdown: drain any remaining spans before exit.
    log!("pg_otel_tracer: draining remaining spans before shutdown");
    unsafe {
        let queue = get_queue();
        while let Some(data) = queue_pop(queue) {
            match serde_json::from_slice::<Vec<crate::span::RawSpan>>(&data) {
                Ok(spans) => batch.extend(spans),
                Err(_) => {}
            }
        }
    }
    if !batch.is_empty() {
        let _ = panic::catch_unwind(|| {
            export_batch_with_retry(&batch)
        });
    }

    log!("pg_otel_tracer: background worker shutting down");
}

/// Export with retry and exponential backoff.
fn export_batch_with_retry(spans: &[crate::span::RawSpan]) -> Result<(), Box<dyn std::error::Error>> {
    for attempt in 0..3 {
        match export_batch(spans) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 2 => {
                let delay = Duration::from_millis(100 * 2_u64.pow(attempt));
                warning!(
                    "pg_otel_tracer: export attempt {} failed, retrying in {:?}: {}",
                    attempt + 1,
                    delay,
                    e
                );
                std::thread::sleep(delay);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
