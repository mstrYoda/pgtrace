use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Simplified span representation that can be cheaply serialized
/// and sent across shared memory to the background worker.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawSpan {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub start_time_unix_nano: u64,
    pub end_time_unix_nano: u64,
    pub attributes: Vec<(String, String)>,
    pub events: Vec<RawEvent>,
    pub status: SpanStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawEvent {
    pub name: String,
    pub timestamp_unix_nano: u64,
    pub attributes: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SpanStatus {
    Unset,
    Ok,
    Error,
}

/// Current monotonic time in nanoseconds since UNIX epoch.
pub fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// Generate a random 16-hex-character span ID.
pub fn generate_span_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

/// Generate a random 32-hex-character trace ID.
pub fn generate_trace_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}
