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
}

#[derive(Debug)]
struct Query {
    model_filter: Option<String>,
    success_filter: Option<bool>,
    latency_min: Option<u64>,
    error_pattern: Option<String>,
    tool_calls_filter: Option<bool>,
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
            if log.latency_ms < min_latency { min_latency = log.latency_ms; }
            if log.latency_ms > max_latency { max_latency = log.latency_ms; }
            
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
        if count > 0 {
            println!("Avg latency: {}ms", total_latency / count);
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
