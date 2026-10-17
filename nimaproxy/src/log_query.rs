//! Turn log query engine - search and analyze nimaproxy turn logs
//!
//! Usage: nimaproxy-query --path /var/log/nimaproxy/turns.jsonl "query"
//!
//! Queries:
//!   model=auto          - Filter by model
//!   success=false       - Filter failed requests
//!   latency>5000        - Filter by latency (ms)
//!   error~"DEGRADED"    - Regex match on error field
//!   has_tool_calls=true - Filter tool call presence
//!   failure_mode=truncated - Exact match on the failure classifier's label
//!   record_kind=completion - Exact match: ttfb (first byte) or completion
//!   count               - Count matching entries
//!   latest              - Show most recent entry
//!   stats               - Show aggregated statistics

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TurnLog {
    pub timestamp: DateTime<Utc>,
    pub requested_model: String,
    pub responding_model: String,
    pub latency_ms: u64,
    pub success: bool,
    pub status_code: u16,
    pub has_tool_calls: bool,
    pub tool_call_count: usize,
    pub error: Option<String>,
    pub is_racing: bool,
    #[serde(default)]
    pub failure_mode: Option<String>,
    #[serde(default = "default_record_kind")]
    pub record_kind: String,
}

/// Default `record_kind` for records written before the field existed.
fn default_record_kind() -> String {
    "ttfb".to_string()
}

#[derive(Debug)]
struct Query {
    model_filter: Option<String>,
    success_filter: Option<bool>,
    latency_min: Option<u64>,
    error_pattern: Option<String>,
    tool_calls_filter: Option<bool>,
    failure_mode_filter: Option<String>,
    record_kind_filter: Option<String>,
    is_count: bool,
    is_latest: bool,
    is_stats: bool,
}

impl Query {
    fn parse(query_str: &str) -> Self {
        let mut q = Query {
            model_filter: None,
            success_filter: None,
            latency_min: None,
            error_pattern: None,
            tool_calls_filter: None,
            failure_mode_filter: None,
            record_kind_filter: None,
            is_count: query_str.trim() == "count",
            is_latest: query_str.trim() == "latest",
            is_stats: query_str.trim() == "stats",
        };

        for part in query_str.split_whitespace() {
            if let Some(model) = part.strip_prefix("model=") {
                q.model_filter = Some(model.to_string());
            } else if let Some(val) = part.strip_prefix("success=") {
                q.success_filter = Some(val == "true");
            } else if let Some(val) = part.strip_prefix("has_tool_calls=") {
                q.tool_calls_filter = Some(val == "true");
            } else if let Some(val) = part.strip_prefix("failure_mode=") {
                q.failure_mode_filter = Some(val.to_string());
            } else if let Some(val) = part.strip_prefix("record_kind=") {
                q.record_kind_filter = Some(val.to_string());
            } else if let Some(val) = part.strip_prefix("latency>") {
                q.latency_min = val.parse().ok();
            } else if let Some(pattern) = part.strip_prefix("error~") {
                q.error_pattern = Some(pattern.trim().to_string());
            }
        }

        q
    }

    fn matches(&self, log: &TurnLog) -> bool {
        if let Some(ref model) = self.model_filter {
            if !log.responding_model.contains(model) {
                return false;
            }
        }
        if let Some(success) = self.success_filter {
            if log.success != success {
                return false;
            }
        }
        if let Some(min) = self.latency_min {
            if log.latency_ms < min {
                return false;
            }
        }
        if let Some(ref pattern) = self.error_pattern {
            if let Some(ref error) = log.error {
                if !error.contains(pattern) {
                    return false;
                }
            } else {
                return false;
            }
        }
        if let Some(has_tools) = self.tool_calls_filter {
            if log.has_tool_calls != has_tools {
                return false;
            }
        }
        // Exact match, like success/has_tool_calls: `failure_mode` is a
        // classifier's label, not prose, so a substring hit would let
        // "truncated" match "truncated_upstream_error" and count two
        // different outcomes as one.
        if let Some(failure_mode) = &self.failure_mode_filter {
            if log.failure_mode.as_deref() != Some(failure_mode.as_str()) {
                return false;
            }
        }
        if let Some(record_kind) = &self.record_kind_filter {
            if log.record_kind != *record_kind {
                return false;
            }
        }
        true
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 3 {
        eprintln!("Usage: nimaproxy-query <log_path> <query>");
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  nimaproxy-query /var/log/nimaproxy/turns.jsonl \"model=mistral\"");
        eprintln!("  nimaproxy-query /var/log/nimaproxy/turns.jsonl \"success=false\"");
        eprintln!("  nimaproxy-query /var/log/nimaproxy/turns.jsonl \"latency>5000\"");
        eprintln!("  nimaproxy-query /var/log/nimaproxy/turns.jsonl \"error~=DEGRADED\"");
        eprintln!("  nimaproxy-query /var/log/nimaproxy/turns.jsonl \"stats\"");
        std::process::exit(1);
    }

    let path = &args[1];
    let query_str = &args[2..].join(" ");
    let query = Query::parse(query_str);

    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error opening log file {}: {}", path, e);
            std::process::exit(1);
        }
    };

    let reader = BufReader::new(file);
    let mut count = 0u64;
    let mut total_latency = 0u64;
    let mut min_latency = u64::MAX;
    let mut max_latency = 0u64;
    let mut errors: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut models: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut latest: Option<TurnLog> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let log: TurnLog = match serde_json::from_str(&line) {
            Ok(l) => l,
            Err(_) => continue,
        };

        if query.matches(&log) {
            count += 1;
            total_latency += log.latency_ms;
            if log.latency_ms < min_latency {
                min_latency = log.latency_ms;
            }
            if log.latency_ms > max_latency {
                max_latency = log.latency_ms;
            }

            if let Some(ref err) = log.error {
                *errors.entry(err.clone()).or_insert(0) += 1;
            }

            *models.entry(log.responding_model.clone()).or_insert(0) += 1;

            latest = Some(log);
        }
    }

    if query.is_count {
        println!("{}", count);
        return;
    }

    if query.is_stats || query_str.trim().is_empty() {
        println!("=== Turn Log Statistics ===");
        println!("Total requests: {}", count);
        if let Some(avg) = total_latency.checked_div(count) {
            println!("Avg latency: {}ms", avg);

            println!("Min latency: {}ms", min_latency);
            println!("Max latency: {}ms", max_latency);

            if !errors.is_empty() {
                println!("\nErrors:");
                let mut sorted: Vec<_> = errors.iter().collect();
                sorted.sort_by(|a, b| b.1.cmp(a.1));
                for (err, cnt) in sorted.iter().take(10) {
                    println!("  {}: {}", cnt, err);
                }
            }

            if !models.is_empty() {
                println!("\nTop models:");
                let mut sorted: Vec<_> = models.iter().collect();
                sorted.sort_by(|a, b| b.1.cmp(a.1));
                for (model, cnt) in sorted.iter().take(10) {
                    println!("  {}: {}", cnt, model);
                }
            }
        }
        return;
    }

    if query.is_latest {
        if let Some(log) = latest {
            println!("{}", serde_json::to_string(&log).unwrap());
        }
        return;
    }

    // Default: show matching entries
    println!("Found {} matching entries", count);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(failure_mode: Option<&str>, record_kind: &str) -> TurnLog {
        TurnLog {
            timestamp: DateTime::parse_from_rfc3339("2026-10-16T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            requested_model: "test/model".to_string(),
            responding_model: "test/model".to_string(),
            latency_ms: 100,
            success: true,
            status_code: 200,
            has_tool_calls: false,
            tool_call_count: 0,
            error: None,
            is_racing: false,
            failure_mode: failure_mode.map(String::from),
            record_kind: record_kind.to_string(),
        }
    }

    #[test]
    fn parses_new_filters() {
        let q = Query::parse("failure_mode=truncated record_kind=completion");
        assert_eq!(q.failure_mode_filter.as_deref(), Some("truncated"));
        assert_eq!(q.record_kind_filter.as_deref(), Some("completion"));
    }

    #[test]
    fn failure_mode_filter_is_an_exact_match() {
        let q = Query::parse("failure_mode=truncated");
        assert!(q.matches(&log(Some("truncated"), "completion")));
        // A classifier label that merely contains the filter must not match:
        // "truncated" and "truncated_upstream_error" are different outcomes.
        assert!(!q.matches(&log(Some("truncated_upstream_error"), "completion")));
        // A record with no classifier label cannot match a label filter.
        assert!(!q.matches(&log(None, "completion")));
    }

    #[test]
    fn failure_mode_filter_does_not_leak_into_other_queries() {
        // A query that does not name the filter must accept records that carry
        // the field and records that do not.
        let q = Query::parse("model=test");
        assert!(q.matches(&log(Some("truncated"), "completion")));
        assert!(q.matches(&log(None, "ttfb")));
    }

    #[test]
    fn record_kind_filter_separates_the_two_halves_of_a_turn() {
        let q = Query::parse("record_kind=completion");
        assert!(q.matches(&log(None, "completion")));
        assert!(!q.matches(&log(None, "ttfb")));
    }

    #[test]
    fn both_filters_combine() {
        let q = Query::parse("record_kind=completion failure_mode=truncated");
        assert!(q.matches(&log(Some("truncated"), "completion")));
        assert!(!q.matches(&log(Some("truncated"), "ttfb")));
        assert!(!q.matches(&log(Some("ok"), "completion")));
    }

    #[test]
    fn old_records_without_the_new_fields_still_parse_and_match() {
        // A record written before the fields existed must deserialize with the
        // documented defaults, or the filter surface would silently drop every
        // pre-existing line of the log.
        let raw = r#"{"timestamp":"2026-10-16T00:00:00Z","requested_model":"m","responding_model":"m","latency_ms":1,"success":true,"status_code":200,"has_tool_calls":false,"tool_call_count":0,"error":null,"is_racing":false}"#;
        let parsed: TurnLog = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.record_kind, "ttfb");
        assert_eq!(parsed.failure_mode, None);
        assert!(Query::parse("record_kind=ttfb").matches(&parsed));
        assert!(Query::parse("model=m").matches(&parsed));
    }
}
