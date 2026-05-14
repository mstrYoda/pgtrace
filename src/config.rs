use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// ─────────────────────────────────────────────────────────────
// Runtime Configuration
// ─────────────────────────────────────────────────────────────
// These are NOT Postgres GUCs yet — they are checked at runtime
// from environment variables on startup and can be toggled via
// a lightweight mechanism.  Full GUC integration is a future
// enhancement requiring pgrx GUC wrapper support.

/// Master on/off switch.  Checked at the top of every hook.
static ENABLED: AtomicBool = AtomicBool::new(true);

/// Sampling rate as billionths (0..1_000_000_000).
/// 1_000_000_000 = trace every query, 0 = trace none.
static SAMPLE_RATE_BILLIONTHS: AtomicU64 = AtomicU64::new(1_000_000_000);

/// Timestamp (nanos since epoch) of last queue-full warning.
static LAST_QUEUE_WARNING: AtomicU64 = AtomicU64::new(0);

/// Minimum interval between identical warnings (1 minute).
const WARNING_INTERVAL_NANOS: u64 = 60_000_000_000;

/// Check the global enabled flag.
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Toggle tracing on or off.
pub fn set_enabled(v: bool) {
    ENABLED.store(v, Ordering::Relaxed);
}

/// Set sampling rate [0.0, 1.0].
pub fn set_sample_rate(rate: f64) {
    let clamped = rate.clamp(0.0, 1.0);
    let billionths = (clamped * 1_000_000_000.0) as u64;
    SAMPLE_RATE_BILLIONTHS.store(billionths, Ordering::Relaxed);
}

/// Get current sampling rate [0.0, 1.0].
pub fn get_sample_rate() -> f64 {
    let billionths = SAMPLE_RATE_BILLIONTHS.load(Ordering::Relaxed);
    (billionths as f64) / 1_000_000_000.0
}

/// Decide probabilistically whether to trace this query.
pub fn should_sample() -> bool {
    let rate = SAMPLE_RATE_BILLIONTHS.load(Ordering::Relaxed);
    if rate >= 1_000_000_000 {
        return true;
    }
    if rate == 0 {
        return false;
    }
    let roll = rand::random::<u64>() % 1_000_000_000;
    roll < rate
}

/// Rate-limited warning for queue-full events.
pub fn maybe_warn_queue_full() {
    let now = crate::span::now_nanos();
    let last = LAST_QUEUE_WARNING.load(Ordering::Relaxed);
    if now.saturating_sub(last) > WARNING_INTERVAL_NANOS {
        // Best-effort CAS to prevent multiple backends from logging simultaneously
        if LAST_QUEUE_WARNING
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            pgrx::warning!("pg_otel_tracer: shared memory queue full, spans dropped");
        }
    }
}
