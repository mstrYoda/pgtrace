use crate::span::{RawSpan, SpanStatus};

/// Export a batch of spans to the configured OTLP/HTTP endpoint.
pub fn export_batch(spans: &[RawSpan]) -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = get_endpoint();
    if endpoint.is_empty() {
        return Ok(());
    }

    let payload = build_otlp_payload(spans);

    let response = ureq::post(&endpoint)
        .set("Content-Type", "application/json")
        .send_string(&payload)?;

    let status = response.status();
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(format!("OTLP export returned HTTP {}", status).into())
    }
}

/// Resolve the collector endpoint.
fn get_endpoint() -> String {
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4318/v1/traces".into())
}

/// Build a minimal OTLP/HTTP JSON trace payload.
fn build_otlp_payload(spans: &[RawSpan]) -> String {
    let resource_attrs = serde_json::json!([
        { "key": "service.name", "value": { "stringValue": "postgresql" } },
        { "key": "db.system",    "value": { "stringValue": "postgresql" } },
        { "key": "service.version", "value": { "stringValue": env!("CARGO_PKG_VERSION") } }
    ]);

    let otlp_spans: Vec<_> = spans.iter().map(|s| {
        let parent = s.parent_span_id.as_ref().map(|p| serde_json::json!(p));
        let status_code = match s.status {
            SpanStatus::Unset => 0,
            SpanStatus::Ok    => 1,
            SpanStatus::Error => 2,
        };

        serde_json::json!({
            "traceId": s.trace_id,
            "spanId": s.span_id,
            "parentSpanId": parent,
            "name": s.name,
            "kind": 1, // SPAN_KIND_INTERNAL
            "startTimeUnixNano": s.start_time_unix_nano.to_string(),
            "endTimeUnixNano": s.end_time_unix_nano.to_string(),
            "attributes": s.attributes.iter().map(|(k, v)| {
                serde_json::json!({ "key": k, "value": { "stringValue": v } })
            }).collect::<Vec<_>>(),
            "events": s.events.iter().map(|e| {
                serde_json::json!({
                    "name": e.name,
                    "timeUnixNano": e.timestamp_unix_nano.to_string(),
                    "attributes": e.attributes.iter().map(|(k, v)| {
                        serde_json::json!({ "key": k, "value": { "stringValue": v } })
                    }).collect::<Vec<_>>(),
                })
            }).collect::<Vec<_>>(),
            "status": { "code": status_code }
        })
    }).collect();

    serde_json::json!({
        "resourceSpans": [
            {
                "resource": { "attributes": resource_attrs },
                "scopeSpans": [
                    {
                        "scope": {
                            "name": "pg_otel_tracer",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "spans": otlp_spans
                    }
                ]
            }
        ]
    }).to_string()
}
