use pgrx::prelude::*;
use std::ffi::CStr;

/// Decoded Postgres wait-event information.
#[derive(Clone, Debug)]
pub struct WaitEventInfo {
    pub event_type: String,
    pub event_name: String,
}

/// Sample the current backend's wait event (if any).
///
/// Returns `None` when the backend is not waiting.
///
/// # Safety
/// Must only be called from a backend process where `MyProc` is valid.
pub unsafe fn sample_wait_event() -> Option<WaitEventInfo> {
    if pg_sys::MyProc.is_null() {
        return None;
    }

    let wait_event_info = (*pg_sys::MyProc).wait_event_info;
    if wait_event_info == 0 {
        return None;
    }

    let event_type = pgstat_get_wait_event_type(wait_event_info);
    let event_name = pgstat_get_wait_event(wait_event_info);

    Some(WaitEventInfo { event_type, event_name })
}

// ─────────────────────────────────────────────────────────────
// Internal helpers (fallback-safe)
// ─────────────────────────────────────────────────────────────

#[inline]
unsafe fn pgstat_get_wait_event_type(info: u32) -> String {
    // Try the C helper if it exists in pgrx bindings.
    let ptr = pg_sys::pgstat_get_wait_event_type(info);
    if !ptr.is_null() {
        return CStr::from_ptr(ptr).to_string_lossy().into_owned();
    }
    // Manual decode of the event class (lower 8 bits).
    let class = info & 0xFF;
    match class {
        0x00 => "Activity".into(),
        0x01 => "BufferPin".into(),
        0x02 => "Client".into(),
        0x03 => "Extension".into(),
        0x04 => "IPC".into(),
        0x05 => "Timeout".into(),
        0x06 => "IO".into(),
        0x07 => "Lock".into(),
        0x08 => "LWLock".into(),
        _ => format!("class_{}", class),
    }
}

#[inline]
unsafe fn pgstat_get_wait_event(info: u32) -> String {
    let ptr = pg_sys::pgstat_get_wait_event(info);
    if !ptr.is_null() {
        return CStr::from_ptr(ptr).to_string_lossy().into_owned();
    }
    // Return the raw hex code when the detailed name is unavailable.
    format!("0x{:08x}", info)
}
