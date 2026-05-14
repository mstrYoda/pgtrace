use lazy_static::lazy_static;
use regex::Regex;

/// Parsed W3C traceparent context.
#[derive(Clone, Debug)]
pub struct TraceContext {
    pub trace_id: String,
    pub parent_span_id: String,
    pub trace_flags: u8,
}

lazy_static! {
    /// Matches sqlcommenter-style traceparent comments.
    /// Examples:
    ///   /*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/
    ///   /* traceparent="00-..." */
    static ref TRACEPARENT_RE: Regex = Regex::new(
        r#"traceparent=['\"]?(00-[0-9a-fA-F]{32}-[0-9a-fA-F]{16}-[0-9a-fA-F]{2})['\"]?"#
    ).unwrap();
}

/// Extract a W3C traceparent from a SQL comment string.
pub fn extract_traceparent(sql: &str) -> Option<TraceContext> {
    let caps = TRACEPARENT_RE.captures(sql)?;
    let tp = caps.get(1)?.as_str();
    let parts: Vec<&str> = tp.split('-').collect();
    if parts.len() != 4 || parts[0] != "00" {
        return None;
    }
    Some(TraceContext {
        trace_id: parts[1].to_lowercase(),
        parent_span_id: parts[2].to_lowercase(),
        trace_flags: u8::from_str_radix(parts[3], 16).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_traceparent() {
        let sql = "SELECT 1 /*traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/";
        let ctx = extract_traceparent(sql).unwrap();
        assert_eq!(ctx.trace_id, "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(ctx.parent_span_id, "b7ad6b7169203331");
        assert_eq!(ctx.trace_flags, 1);
    }

    #[test]
    fn test_double_quoted_traceparent() {
        let sql = r#"SELECT * FROM users /*traceparent="00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"*/"#;
        let ctx = extract_traceparent(sql).unwrap();
        assert_eq!(ctx.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
    }

    #[test]
    fn test_no_traceparent() {
        let sql = "SELECT 1";
        assert!(extract_traceparent(sql).is_none());
    }
}
