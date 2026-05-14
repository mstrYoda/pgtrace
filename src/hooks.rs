use pgrx::prelude::*;
use std::ffi::CStr;

use crate::parser::{extract_traceparent, TraceContext};
use crate::shared::{get_queue, queue_push};
use crate::span::*;
use crate::wait_events::sample_wait_event;

// ─────────────────────────────────────────────────────────────
// Saved previous hooks (explicit types for pgrx portability)
// ─────────────────────────────────────────────────────────────

static mut PREV_PLANNER_HOOK: Option<
    unsafe extern "C" fn(
        *mut pg_sys::Query,
        *const ::core::ffi::c_char,
        i32,
        pg_sys::ParamListInfo,
    ) -> *mut pg_sys::PlannedStmt,
> = None;

static mut PREV_EXECUTOR_START_HOOK: Option<
    unsafe extern "C" fn(*mut pg_sys::QueryDesc, i32),
> = None;

static mut PREV_EXECUTOR_RUN_HOOK: Option<
    unsafe extern "C" fn(*mut pg_sys::QueryDesc, pg_sys::ScanDirection, u64, bool),
> = None;

static mut PREV_EXECUTOR_END_HOOK: Option<
    unsafe extern "C" fn(*mut pg_sys::QueryDesc),
> = None;

static mut PREV_SHMEM_STARTUP_HOOK: Option<unsafe extern "C" fn()> = None;
static mut PREV_SHMEM_REQUEST_HOOK: Option<unsafe extern "C" fn()> = None;

// ─────────────────────────────────────────────────────────────
// Per-backend thread-local state
// ─────────────────────────────────────────────────────────────

thread_local! {
    static TRACE_CTX: std::cell::RefCell<Option<TraceContext>> = std::cell::RefCell::new(None);
    static SPAN_BUFFER: std::cell::RefCell<Vec<RawSpan>> = std::cell::RefCell::new(Vec::new());
    static PLANNER_SPAN: std::cell::RefCell<Option<RawSpan>> = std::cell::RefCell::new(None);
    static EXEC_SPAN: std::cell::RefCell<Option<RawSpan>> = std::cell::RefCell::new(None);
    static RUN_SPAN: std::cell::RefCell<Option<RawSpan>> = std::cell::RefCell::new(None);
}

// ─────────────────────────────────────────────────────────────
// Hook installation
// ─────────────────────────────────────────────────────────────

pub unsafe fn install_shmem_request_hook() {
    PREV_SHMEM_REQUEST_HOOK = pg_sys::shmem_request_hook;
    pg_sys::shmem_request_hook = Some(shmem_request_hook);
}

pub unsafe fn install_shmem_hook() {
    PREV_SHMEM_STARTUP_HOOK = pg_sys::shmem_startup_hook;
    pg_sys::shmem_startup_hook = Some(shmem_startup_hook);
}

pub unsafe fn install_query_hooks() {
    PREV_PLANNER_HOOK = pg_sys::planner_hook;
    pg_sys::planner_hook = Some(planner_hook);

    PREV_EXECUTOR_START_HOOK = pg_sys::ExecutorStart_hook;
    pg_sys::ExecutorStart_hook = Some(executor_start_hook);

    PREV_EXECUTOR_RUN_HOOK = pg_sys::ExecutorRun_hook;
    pg_sys::ExecutorRun_hook = Some(executor_run_hook);

    PREV_EXECUTOR_END_HOOK = pg_sys::ExecutorEnd_hook;
    pg_sys::ExecutorEnd_hook = Some(executor_end_hook);
}

// ─────────────────────────────────────────────────────────────
// Shared-memory startup hook
// ─────────────────────────────────────────────────────────────

#[pg_guard]
unsafe extern "C" fn shmem_startup_hook() {
    if let Some(prev) = PREV_SHMEM_STARTUP_HOOK {
        prev();
    }

    let queue = crate::shared::init_queue();
    crate::shared::set_queue(queue);
}

// ─────────────────────────────────────────────────────────────
// Shared-memory request hook (PG15+)
// ─────────────────────────────────────────────────────────────

#[pg_guard]
unsafe extern "C" fn shmem_request_hook() {
    if let Some(prev) = PREV_SHMEM_REQUEST_HOOK {
        prev();
    }

    pg_sys::RequestAddinShmemSpace(crate::shared::shared_queue_size() as _);
}

// ─────────────────────────────────────────────────────────────
// Planner hook
// ─────────────────────────────────────────────────────────────

#[pg_guard]
unsafe extern "C" fn planner_hook(
    parse: *mut pg_sys::Query,
    query_string: *const ::core::ffi::c_char,
    cursor_options: i32,
    bound_params: pg_sys::ParamListInfo,
) -> *mut pg_sys::PlannedStmt {
    let qstr = if query_string.is_null() {
        String::new()
    } else {
        CStr::from_ptr(query_string).to_string_lossy().into_owned()
    };

    if let Some(ctx) = extract_traceparent(&qstr) {
        let span = start_span("planner", &ctx, None);
        PLANNER_SPAN.with(|s| *s.borrow_mut() = Some(span));
        TRACE_CTX.with(|t| *t.borrow_mut() = Some(ctx));
    }

    let result = if let Some(prev) = PREV_PLANNER_HOOK {
        prev(parse, query_string, cursor_options, bound_params)
    } else {
        pg_sys::standard_planner(parse, query_string, cursor_options, bound_params)
    };

    PLANNER_SPAN.with(|s| {
        if let Some(span) = s.borrow_mut().take() {
            finish_span(span);
        }
    });

    result
}

// ─────────────────────────────────────────────────────────────
// ExecutorStart hook
// ─────────────────────────────────────────────────────────────

#[pg_guard]
unsafe extern "C" fn executor_start_hook(query_desc: *mut pg_sys::QueryDesc, eflags: i32) {
    if !query_desc.is_null() {
        let source_text = if (*query_desc).sourceText.is_null() {
            String::new()
        } else {
            CStr::from_ptr((*query_desc).sourceText)
                .to_string_lossy()
                .into_owned()
        };

        let ctx = TRACE_CTX.with(|t| t.borrow().clone());
        let ctx = match ctx {
            Some(c) => c,
            None => {
                if let Some(c) = extract_traceparent(&source_text) {
                    TRACE_CTX.with(|t| *t.borrow_mut() = Some(c.clone()));
                    c
                } else {
                    // No trace context — passthrough.
                    if let Some(prev) = PREV_EXECUTOR_START_HOOK {
                        prev(query_desc, eflags);
                    } else {
                        pg_sys::standard_ExecutorStart(query_desc, eflags);
                    }
                    return;
                }
            }
        };

        let parent_id = PLANNER_SPAN
            .with(|s| s.borrow().as_ref().map(|sp| sp.span_id.clone()))
            .or_else(|| Some(ctx.parent_span_id.clone()));

        let span = start_span("query execution", &ctx, parent_id);
        EXEC_SPAN.with(|s| *s.borrow_mut() = Some(span));
    }

    if let Some(prev) = PREV_EXECUTOR_START_HOOK {
        prev(query_desc, eflags);
    } else {
        pg_sys::standard_ExecutorStart(query_desc, eflags);
    }
}

// ─────────────────────────────────────────────────────────────
// ExecutorRun hook
// ─────────────────────────────────────────────────────────────

#[pg_guard]
unsafe extern "C" fn executor_run_hook(
    query_desc: *mut pg_sys::QueryDesc,
    direction: pg_sys::ScanDirection,
    count: u64,
    execute_once: bool,
) {
    let ctx = TRACE_CTX.with(|t| t.borrow().clone());

    if let Some(ref c) = ctx {
        let exec_span_id = EXEC_SPAN.with(|s| s.borrow().as_ref().map(|sp| sp.span_id.clone()));
        let mut span = start_span("executor run", c, exec_span_id);

        // Sample wait events before execution.
        if let Some(we) = sample_wait_event() {
            add_event(
                &mut span,
                "wait_event_before",
                vec![
                    ("type".into(), we.event_type),
                    ("event".into(), we.event_name),
                ],
            );
        }

        RUN_SPAN.with(|s| *s.borrow_mut() = Some(span));
    }

    if let Some(prev) = PREV_EXECUTOR_RUN_HOOK {
        prev(query_desc, direction, count, execute_once);
    } else {
        pg_sys::standard_ExecutorRun(query_desc, direction, count, execute_once);
    }

    if ctx.is_some() {
        RUN_SPAN.with(|s| {
            if let Some(mut span) = s.borrow_mut().take() {
                // Sample wait events after execution.
                if let Some(we) = sample_wait_event() {
                    add_event(
                        &mut span,
                        "wait_event_after",
                        vec![
                            ("type".into(), we.event_type),
                            ("event".into(), we.event_name),
                        ],
                    );
                }

                // Attach row count from the executor state.
                if !query_desc.is_null() && !(*query_desc).estate.is_null() {
                    let rows = (*(*query_desc).estate).es_processed;
                    span.attributes
                        .push(("db.row_count".into(), rows.to_string()));
                }

                finish_span(span);
            }
        });
    }
}

// ─────────────────────────────────────────────────────────────
// ExecutorEnd hook
// ─────────────────────────────────────────────────────────────

#[pg_guard]
unsafe extern "C" fn executor_end_hook(query_desc: *mut pg_sys::QueryDesc) {
    let ctx = TRACE_CTX.with(|t| t.borrow().clone());

    if ctx.is_some() {
        EXEC_SPAN.with(|s| {
            if let Some(span) = s.borrow_mut().take() {
                finish_span(span);
            }
        });

        // Flush the accumulated span batch to shared memory.
        SPAN_BUFFER.with(|buf| {
            let spans = buf.borrow_mut().drain(..).collect::<Vec<_>>();
            if !spans.is_empty() {
                match serde_json::to_vec(&spans) {
                    Ok(data) => {
                        let queue = get_queue();
                        if !queue.is_null() {
                            if !queue_push(queue, &data) {
                                warning!(
                                    "pg_otel_tracer: shared memory queue full, spans dropped"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        warning!(
                            "pg_otel_tracer: failed to serialize spans: {}",
                            e
                        );
                    }
                }
            }
        });

        TRACE_CTX.with(|t| *t.borrow_mut() = None);
    }

    if let Some(prev) = PREV_EXECUTOR_END_HOOK {
        prev(query_desc);
    } else {
        pg_sys::standard_ExecutorEnd(query_desc);
    }
}

// ─────────────────────────────────────────────────────────────
// Span helpers
// ─────────────────────────────────────────────────────────────

fn start_span(name: &str, ctx: &TraceContext, parent_span_id: Option<String>) -> RawSpan {
    RawSpan {
        trace_id: ctx.trace_id.clone(),
        span_id: generate_span_id(),
        parent_span_id: parent_span_id.or_else(|| Some(ctx.parent_span_id.clone())),
        name: name.into(),
        start_time_unix_nano: now_nanos(),
        end_time_unix_nano: 0,
        attributes: vec![],
        events: vec![],
        status: SpanStatus::Unset,
    }
}

fn finish_span(mut span: RawSpan) {
    span.end_time_unix_nano = now_nanos();
    SPAN_BUFFER.with(|buf| {
        buf.borrow_mut().push(span);
    });
}

fn add_event(span: &mut RawSpan, name: &str, attrs: Vec<(String, String)>) {
    span.events.push(RawEvent {
        name: name.into(),
        timestamp_unix_nano: now_nanos(),
        attributes: attrs,
    });
}
