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
//!   response_tool_calls>=2 - Filter by RESPONSE-side distinct tool calls
//!   failure_mode=truncated - Exact match on the failure classifier's label
//!   record_kind=completion - Exact match: ttfb (first byte) or completion
//!   session=<id>        - Exact match on the agent-loop session id
//!   loop=3              - Exact match on the loop (user turn) number within its session
//!   round>=5            - Rounds at or after N within a loop
//!   count               - Count matching entries
//!   latest              - Show most recent entry
//!   stats               - Show aggregated statistics
//!   loops               - List agent loops with depth, duration and tool calls
//!   depth               - Percentiles and histogram of agent-loop depth
//!
//! `loops` and `depth` read the `loop` records the proxy writes when a user
//! turn ends. A loop record is not a request, so the request-level filters
//! (`model=`, `success=`, `latency>`, `error~`, `has_tool_calls=`,
//! `failure_mode=`) are deliberately NOT applied to it — see
//! [`Query::matches_loop`]. Select loops with `session=` and `loop=`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};

/// Identity of an agent loop on a request record.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LoopKey {
    pub session_id: String,
    pub loop_seq: u64,
    pub round_index: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TurnLog {
    pub timestamp: DateTime<Utc>,
    pub requested_model: String,
    pub responding_model: String,
    pub latency_ms: u64,
    pub success: bool,
    pub status_code: u16,
    pub has_tool_calls: bool,
    /// REQUEST-SIDE tool history count. Overcounts what the model did: it
    /// counts the tool messages and `tool_calls` entries the request carried,
    /// so a turn whose response made no tool call still logs a non-zero value
    /// here. Use `response_tool_calls` for a rate that means anything.
    pub tool_call_count: usize,
    /// The REQUEST-side count under a name that says which side it is -- the
    /// same number as `tool_call_count`, which keeps its historical meaning.
    /// `None` on a line written before the field existed.
    #[serde(default)]
    pub request_tool_call_count: Option<u64>,
    /// Distinct logical tool calls the RESPONSE made, de-duplicated by the SSE
    /// delta's `index`. `Some(0)` means the response genuinely made no tool
    /// call; `None` means the response was not observed (first-byte record,
    /// failure path, non-streaming turn, or a line written before the field
    /// existed).
    #[serde(default)]
    pub response_tool_calls: Option<u64>,
    pub error: Option<String>,
    pub is_racing: bool,
    #[serde(default)]
    pub failure_mode: Option<String>,
    #[serde(default = "default_record_kind")]
    pub record_kind: String,

    /// Set on request records that belong to a tracked loop, including the
    /// proxy's failure-path records: a failed attempt names the turn it
    /// belongs to, so a session filter returns that conversation's failures
    /// alongside its successes. Absent only on records written before loop
    /// tracking existed and on requests that never resolved a session.
    #[serde(default)]
    pub loop_key: Option<LoopKey>,

    /// Present only on a `loop` summary record: the loop's own span, not the
    /// span of the request that happened to close it.
    #[serde(default)]
    pub loop_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub loop_ended_at: Option<DateTime<Utc>>,
    /// Rounds the loop STARTED. A round is counted when its request begins, so
    /// a round that failed before reporting an outcome is still in here.
    #[serde(default)]
    pub loop_rounds: Option<u64>,
    /// Rounds that reported an outcome. `loop_rounds - loop_completed_rounds`
    /// is exactly the number of rounds that started and never reported, and
    /// `loop_model_ms` covers only this smaller set — an unknown duration is
    /// left unknown rather than estimated.
    #[serde(default)]
    pub loop_completed_rounds: Option<u64>,
    /// Summed upstream time across completed rounds only.
    #[serde(default)]
    pub loop_model_ms: Option<u64>,
    /// `loop_ended_at - loop_started_at`: what the user actually waited.
    /// The gap between this and `loop_model_ms` is tool execution plus client
    /// time, which is the whole reason both numbers are stored.
    #[serde(default)]
    pub loop_wall_ms: Option<u64>,
    #[serde(default)]
    pub loop_error_rounds: Option<u64>,
    /// Fragment entries, i.e. SSE frames carrying tool-call deltas. Overcounts
    /// logical tool calls; use `loop_tool_calls_distinct` for the real count.
    #[serde(default)]
    pub loop_tool_call_fragments: Option<u64>,
    /// Distinct logical tool calls, keyed by the delta's `index`.
    #[serde(default)]
    pub loop_tool_calls_distinct: Option<u64>,
    #[serde(default)]
    pub loop_last_finish_reason: Option<String>,
    /// `new_turn` (the client started another turn) or `idle` (the sweeper
    /// closed it after silence).
    #[serde(default)]
    pub loop_ended_by: Option<String>,
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
    /// Minimum RESPONSE-side distinct tool calls. Separate from
    /// `tool_calls_filter`, which filters the request-side `has_tool_calls`
    /// bool.
    response_tool_calls_min: Option<u64>,
    failure_mode_filter: Option<String>,
    record_kind_filter: Option<String>,
    session_filter: Option<String>,
    loop_filter: Option<u64>,
    round_min: Option<u64>,
    is_count: bool,
    is_latest: bool,
    is_stats: bool,
    is_loops: bool,
    is_depth: bool,
}

impl Query {
    fn parse(query_str: &str) -> Self {
        let mut q = Query {
            model_filter: None,
            success_filter: None,
            latency_min: None,
            error_pattern: None,
            tool_calls_filter: None,
            response_tool_calls_min: None,
            failure_mode_filter: None,
            record_kind_filter: None,
            session_filter: None,
            loop_filter: None,
            round_min: None,
            is_count: query_str.trim() == "count",
            is_latest: query_str.trim() == "latest",
            is_stats: query_str.trim() == "stats",
            is_loops: query_str.trim() == "loops",
            is_depth: query_str.trim() == "depth",
        };

        for part in query_str.split_whitespace() {
            if let Some(model) = part.strip_prefix("model=") {
                q.model_filter = Some(model.to_string());
            } else if let Some(val) = part.strip_prefix("success=") {
                q.success_filter = Some(val == "true");
            } else if let Some(val) = part.strip_prefix("has_tool_calls=") {
                q.tool_calls_filter = Some(val == "true");
            } else if let Some(val) = part.strip_prefix("response_tool_calls>=") {
                q.response_tool_calls_min = val.parse().ok();
            } else if let Some(val) = part.strip_prefix("failure_mode=") {
                q.failure_mode_filter = Some(val.to_string());
            } else if let Some(val) = part.strip_prefix("record_kind=") {
                q.record_kind_filter = Some(val.to_string());
            } else if let Some(val) = part.strip_prefix("session=") {
                q.session_filter = Some(val.to_string());
            } else if let Some(val) = part.strip_prefix("loop=") {
                q.loop_filter = val.parse().ok();
            } else if let Some(val) = part.strip_prefix("round>=") {
                q.round_min = val.parse().ok();
            } else if let Some(val) = part.strip_prefix("latency>") {
                q.latency_min = val.parse().ok();
            } else if let Some(pattern) = part.strip_prefix("error~") {
                q.error_pattern = Some(pattern.trim().to_string());
            }
            // Recognised here rather than only via the whole-string test above,
            // so a mode can be combined with a filter: `session=fp:ab depth`.
            if part == "loops" {
                q.is_loops = true;
            } else if part == "depth" {
                q.is_depth = true;
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
        // Response-side, unlike `has_tool_calls=`/`tool_call_count`: this is
        // the count of logical tool calls the response actually made. A record
        // whose response was never observed (`None`) cannot satisfy the filter
        // and must not be silently included — an absent count is "unknown",
        // not "at least N". So `response_tool_calls>=1` is the query that finds
        // the turns that really did call a tool, which the request-side field
        // cannot answer.
        if let Some(min) = self.response_tool_calls_min {
            match log.response_tool_calls {
                Some(n) if n >= min => {}
                _ => return false,
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
        // Loop identity applies to any record carrying one. A record with no
        // `loop_key` cannot satisfy a loop filter and must NOT be silently
        // included: an absent key is "not in this loop", not "any loop".
        if let Some(session) = &self.session_filter {
            match log.loop_key.as_ref() {
                Some(key) if key.session_id == *session => {}
                _ => return false,
            }
        }
        if let Some(loop_seq) = self.loop_filter {
            match log.loop_key.as_ref() {
                Some(key) if key.loop_seq == loop_seq => {}
                _ => return false,
            }
        }
        if let Some(min) = self.round_min {
            match log.loop_key.as_ref() {
                Some(key) if key.round_index >= min => {}
                _ => return false,
            }
        }
        true
    }

    /// Whether a `loop` summary record satisfies the LOOP-level filters.
    ///
    /// Request-level filters are deliberately not consulted here. A loop
    /// summary is not a request: it has no model, no status code and no
    /// latency of its own, and the proxy writes empty/zero placeholders into
    /// those fields. Applying `model=` or `latency>` to it would therefore
    /// either match nothing or match for the wrong reason, and a filter that
    /// silently returns zero rows is worse than one that is documented as not
    /// applying. Loops are selected with `session=`, `loop=` and `round>=`.
    fn matches_loop(&self, log: &TurnLog) -> bool {
        // Mirrors `TurnLog::RECORD_KIND_LOOP` in the writer. The reader keeps
        // its own copy of the schema on purpose, so a field the writer adds
        // cannot break an older reader.
        if log.record_kind != "loop" {
            return false;
        }
        if let Some(kind) = &self.record_kind_filter {
            if kind != "loop" {
                return false;
            }
        }
        if let Some(session) = &self.session_filter {
            match log.loop_key.as_ref() {
                Some(key) if key.session_id == *session => {}
                _ => return false,
            }
        }
        if let Some(loop_seq) = self.loop_filter {
            match log.loop_key.as_ref() {
                Some(key) if key.loop_seq == loop_seq => {}
                _ => return false,
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
    let mut loops: Vec<TurnLog> = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let log: TurnLog = match serde_json::from_str(&line) {
            Ok(l) => l,
            Err(_) => continue,
        };

        // Loop summaries are collected on their own predicate: they carry no
        // request outcome, so the request-level filters above would either
        // exclude every one of them or match for the wrong reason. Only the
        // loop-level filters apply -- see `matches_loop`.
        if (query.is_loops || query.is_depth) && query.matches_loop(&log) {
            loops.push(log.clone());
        }

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

    if query.is_loops || query.is_depth {
        print_loop_report(&mut loops, query.is_loops);
        return;
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

/// Nearest-rank percentile: the smallest value at or above which `p` percent of
/// the sample lies. An empty sample is 0 rather than a panic, so a filtered
/// query that matched no loop still prints a readable report.
fn percentile(sorted: &[u64], p: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p as f64 / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Milliseconds rendered as seconds with one decimal, because a loop's
/// duration is minutes and printing it in ms hides the magnitude.
fn secs(ms: u64) -> String {
    format!("{:.1}", ms as f64 / 1000.0)
}

/// Print the depth report for the matched `loop` records.
fn print_loop_report(loops: &mut [TurnLog], show_table: bool) {
    if loops.is_empty() {
        println!("No loop records matched.");
        println!();
        println!("Loop records (record_kind=loop) are written when a user turn ends,");
        println!("so an empty result means either no turn has ended yet since the");
        println!("proxy was last started, or the running proxy predates loop tracking.");
        println!("Check the whole file with `record_kind=loop count`.");
        return;
    }

    // Order by when the loop STARTED, then by sequence number, so two loops
    // that began in the same millisecond still print in a stable order.
    loops.sort_by(|a, b| {
        (a.loop_started_at, a.loop_key.as_ref().map(|k| k.loop_seq))
            .cmp(&(b.loop_started_at, b.loop_key.as_ref().map(|k| k.loop_seq)))
    });

    if show_table {
        println!(
            "{:<20} {:>5} {:>6} {:>6} {:>8} {:>8} {:>6} {:<10} ended",
            "session", "loop", "rounds", "done", "wall_s", "model_s", "tools", "finish"
        );
        for log in loops.iter() {
            let session = log
                .loop_key
                .as_ref()
                .map(|k| k.session_id.as_str())
                .unwrap_or("-");
            let shown: String = session.chars().take(20).collect();
            println!(
                "{:<20} {:>5} {:>6} {:>6} {:>8} {:>8} {:>6} {:<10} {}",
                shown,
                log.loop_key.as_ref().map(|k| k.loop_seq).unwrap_or(0),
                log.loop_rounds.unwrap_or(0),
                log.loop_completed_rounds.unwrap_or(0),
                secs(log.loop_wall_ms.unwrap_or(0)),
                secs(log.loop_model_ms.unwrap_or(0)),
                log.loop_tool_calls_distinct.unwrap_or(0),
                log.loop_last_finish_reason.as_deref().unwrap_or("-"),
                log.loop_ended_by.as_deref().unwrap_or("-"),
            );
        }
        println!();
    }

    println!("=== Agent Loop Depth ===");
    println!("loops: {}", loops.len());

    for (label, mut values) in [
        (
            "rounds",
            loops
                .iter()
                .filter_map(|l| l.loop_rounds)
                .collect::<Vec<_>>(),
        ),
        (
            "wall_ms",
            loops
                .iter()
                .filter_map(|l| l.loop_wall_ms)
                .collect::<Vec<_>>(),
        ),
        (
            "model_ms",
            loops
                .iter()
                .filter_map(|l| l.loop_model_ms)
                .collect::<Vec<_>>(),
        ),
        (
            "tools_distinct",
            loops
                .iter()
                .filter_map(|l| l.loop_tool_calls_distinct)
                .collect::<Vec<_>>(),
        ),
    ] {
        values.sort_unstable();
        println!(
            "{:<15} n={:<5} p50={:<8} p90={:<8} p99={:<8} max={}",
            label,
            values.len(),
            percentile(&values, 50),
            percentile(&values, 90),
            percentile(&values, 99),
            values.last().copied().unwrap_or(0)
        );
    }

    // The histogram is the point of the whole feature: the SHAPE of the depth
    // distribution is what shows a turn ending shallow. Percentiles alone
    // cannot distinguish a bimodal split between "gave up at round 2" and
    // "worked for 40 rounds" from a uniformly mediocre population.
    let mut rounds: Vec<u64> = loops.iter().filter_map(|l| l.loop_rounds).collect();
    rounds.sort_unstable();
    if !rounds.is_empty() {
        println!();
        println!("=== rounds per loop ===");
        let mut i = 0;
        while i < rounds.len() {
            let value = rounds[i];
            let start = i;
            while i < rounds.len() && rounds[i] == value {
                i += 1;
            }
            let n = i - start;
            println!("{:>4} | {:<40} {}", value, "#".repeat(n.min(40)), n);
        }
    }

    let mut ended: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for log in loops.iter() {
        *ended
            .entry(
                log.loop_ended_by
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            )
            .or_insert(0) += 1;
    }
    println!();
    println!("=== ended_by ===");
    let mut ended: Vec<_> = ended.into_iter().collect();
    ended.sort_by_key(|a| std::cmp::Reverse(a.1));
    for (how, n) in ended {
        println!("  {}: {}", how, n);
    }

    // A loop whose rounds exceed its completed rounds contains requests that
    // started and never reported an outcome -- 504s and hard upstream failures.
    // Their duration is unknown, so such a loop's `model_ms` understates its
    // true model cost. Name them rather than letting the sum look complete.
    let lagging: Vec<_> = loops
        .iter()
        .filter(|l| l.loop_rounds.unwrap_or(0) > l.loop_completed_rounds.unwrap_or(0))
        .collect();
    if !lagging.is_empty() {
        let unreported: u64 = lagging
            .iter()
            .map(|l| l.loop_rounds.unwrap_or(0) - l.loop_completed_rounds.unwrap_or(0))
            .sum();
        println!();
        println!(
            "{} loops have {} rounds that never reported (504 / upstream failure);",
            lagging.len(),
            unreported
        );
        println!("their model_ms covers completed rounds only.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record with every field neutral, so a test that cares about two fields
    /// does not have to restate the other twenty — and so adding a field to the
    /// reader touches this function only.
    fn blank() -> TurnLog {
        TurnLog {
            timestamp: ts("2026-10-16T00:00:00Z"),
            requested_model: "test/model".to_string(),
            responding_model: "test/model".to_string(),
            latency_ms: 100,
            success: true,
            status_code: 200,
            has_tool_calls: false,
            tool_call_count: 0,
            request_tool_call_count: None,
            response_tool_calls: None,
            error: None,
            is_racing: false,
            failure_mode: None,
            record_kind: default_record_kind(),
            loop_key: None,
            loop_started_at: None,
            loop_ended_at: None,
            loop_rounds: None,
            loop_completed_rounds: None,
            loop_model_ms: None,
            loop_wall_ms: None,
            loop_error_rounds: None,
            loop_tool_call_fragments: None,
            loop_tool_calls_distinct: None,
            loop_last_finish_reason: None,
            loop_ended_by: None,
        }
    }

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn log(failure_mode: Option<&str>, record_kind: &str) -> TurnLog {
        TurnLog {
            failure_mode: failure_mode.map(String::from),
            record_kind: record_kind.to_string(),
            ..blank()
        }
    }

    /// A `loop` summary shaped the way the proxy writes it: no request outcome,
    /// and the loop's own span carried in the `loop_*` fields.
    fn loop_log(session: &str, seq: u64, rounds: u64, completed: u64, wall_ms: u64) -> TurnLog {
        TurnLog {
            record_kind: "loop".to_string(),
            requested_model: String::new(),
            responding_model: String::new(),
            loop_key: Some(LoopKey {
                session_id: session.to_string(),
                loop_seq: seq,
                // A summary is not a round, so it carries 0 rather than a
                // fabricated round number.
                round_index: 0,
            }),
            loop_started_at: Some(ts("2026-10-16T00:00:00Z")),
            loop_ended_at: Some(ts("2026-10-16T00:10:00Z")),
            loop_rounds: Some(rounds),
            loop_completed_rounds: Some(completed),
            loop_model_ms: Some(wall_ms / 2),
            loop_wall_ms: Some(wall_ms),
            loop_error_rounds: Some(rounds - completed),
            loop_tool_call_fragments: Some(rounds * 80),
            loop_tool_calls_distinct: Some(rounds * 3),
            loop_last_finish_reason: Some("stop".to_string()),
            loop_ended_by: Some("new_turn".to_string()),
            ..blank()
        }
    }

    #[test]
    fn parses_new_filters() {
        let q = Query::parse("failure_mode=truncated record_kind=completion");
        assert_eq!(q.failure_mode_filter.as_deref(), Some("truncated"));
        assert_eq!(q.record_kind_filter.as_deref(), Some("completion"));
    }

    #[test]
    fn parses_the_response_tool_calls_filter() {
        let q = Query::parse("response_tool_calls>=2 record_kind=completion");
        assert_eq!(q.response_tool_calls_min, Some(2));
        assert_eq!(q.record_kind_filter.as_deref(), Some("completion"));
        // A query that does not name it must leave it unset, so it cannot
        // silently narrow every other query.
        assert_eq!(Query::parse("model=test").response_tool_calls_min, None);
    }

    #[test]
    fn response_tool_calls_filter_selects_the_response_side() {
        // The defect this field fixes: the request carried tool history (so
        // `has_tool_calls` is true and `tool_call_count` is non-zero) but the
        // response made no tool call. A response-side filter must see 0 here.
        let non_call = TurnLog {
            has_tool_calls: true,
            tool_call_count: 4,
            response_tool_calls: Some(0),
            ..blank()
        };
        let called = TurnLog {
            has_tool_calls: true,
            tool_call_count: 4,
            response_tool_calls: Some(3),
            ..blank()
        };
        assert!(!Query::parse("response_tool_calls>=1").matches(&non_call));
        assert!(Query::parse("response_tool_calls>=1").matches(&called));
        // The request-side filter still answers the old question on the same
        // rows: it cannot tell the two apart, which is the point.
        assert!(Query::parse("has_tool_calls=true").matches(&non_call));
        assert!(Query::parse("has_tool_calls=true").matches(&called));
    }

    #[test]
    fn response_tool_calls_filter_rejects_an_unobserved_response() {
        // `None` means "not observed" (first-byte record, failure path, or a
        // line written before the field). It must not pass a `>=1` filter by
        // being coerced to zero, or the query would under-report.
        let unobserved = TurnLog {
            response_tool_calls: None,
            ..blank()
        };
        assert!(!Query::parse("response_tool_calls>=1").matches(&unobserved));
        // `>=0` is satisfied by an observed zero but not by an unobserved one:
        // "at least zero calls were made" requires the calls to have been
        // counted at all.
        let observed_zero = TurnLog {
            response_tool_calls: Some(0),
            ..blank()
        };
        assert!(Query::parse("response_tool_calls>=0").matches(&observed_zero));
        assert!(!Query::parse("response_tool_calls>=0").matches(&unobserved));
    }

    #[test]
    fn a_line_without_the_new_field_still_deserializes_as_none() {
        // Written before this change: no `response_tool_calls` key at all.
        let raw = r#"{"timestamp":"2026-10-16T00:00:00Z","requested_model":"m","responding_model":"m","latency_ms":1,"success":true,"status_code":200,"has_tool_calls":true,"tool_call_count":6,"error":null,"is_racing":false,"record_kind":"completion"}"#;
        let parsed: TurnLog = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.tool_call_count, 6);
        assert_eq!(
            parsed.response_tool_calls, None,
            "an absent key is 'not observed', never 0"
        );
        assert_eq!(
            parsed.request_tool_call_count, None,
            "the request-side alias did not exist on that line either, and an absent count is unknown"
        );
        // The compatibility guarantee: the legacy key keeps its historical
        // REQUEST-side meaning on an old line, so no pre-existing line is
        // reinterpreted by the alias, and the old line correctly carries no
        // response-side count.
        assert_eq!(parsed.tool_call_count, 6);
    }

    /// The three tool-call fields name three different sides, and a line
    /// written before the alias existed still reads the way it always did.
    #[test]
    fn the_tool_call_fields_name_different_sides() {
        let mut rec = log(None, "completion");
        rec.tool_call_count = 6;
        rec.request_tool_call_count = Some(6);
        rec.response_tool_calls = Some(1);
        // The request side is the request's tool history; the response side is
        // what the response made. They are not interchangeable.
        assert_eq!(rec.tool_call_count, 6);
        assert_eq!(rec.request_tool_call_count, Some(6));
        assert_eq!(rec.response_tool_calls, Some(1));
        // The response-side filter answers the response-side question.
        assert!(Query::parse("response_tool_calls>=1").matches(&rec));
        assert!(!Query::parse("response_tool_calls>=2").matches(&rec));
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

    #[test]
    fn session_filter_is_exact_not_a_prefix() {
        let mut rec = log(None, "completion");
        rec.loop_key = Some(LoopKey {
            session_id: "fp:aa".to_string(),
            loop_seq: 1,
            round_index: 1,
        });
        assert!(Query::parse("session=fp:aa").matches(&rec));
        // A prefix must not match: two session ids that share a prefix are two
        // different conversations, and folding them together would merge two
        // loops into one and halve the measured depth.
        assert!(!Query::parse("session=fp:a").matches(&rec));
    }

    #[test]
    fn a_record_without_a_loop_key_cannot_satisfy_a_loop_filter() {
        // A record with no key must fail a session filter rather than pass it
        // by default, or a query for one conversation would silently include
        // every untracked request. This is about keyless records, not about
        // which records are keyless: the proxy's failure-path records now
        // carry their key, and pass legitimately.
        let rec = log(None, "completion");
        assert!(rec.loop_key.is_none());
        assert!(!Query::parse("session=fp:aa").matches(&rec));
        assert!(!Query::parse("loop=1").matches(&rec));
        assert!(!Query::parse("round>=1").matches(&rec));
    }

    /// A failure attempt belongs to a turn, and the turn's key is what makes
    /// the attempt joinable to the attempt that recovered from it. The turn
    /// log's only other handle is `request_id`, which is unique per record,
    /// so a keyed failure record is the difference between a recoverable
    /// attempt and an unattributable one.
    #[test]
    fn a_failure_record_with_a_loop_key_joins_its_turn() {
        let key = LoopKey {
            session_id: "fp:aa".to_string(),
            loop_seq: 2,
            round_index: 3,
        };
        let mut stalled = log(None, "ttfb");
        stalled.loop_key = Some(key.clone());
        stalled.error = Some("upstream sent no bytes within 15000ms".to_string());
        stalled.success = false;
        stalled.status_code = 504;

        let mut recovered = log(Some("clean_stop"), "completion");
        recovered.loop_key = Some(key.clone());

        // Same turn: one key, so the two records are joinable.
        assert_eq!(stalled.loop_key, recovered.loop_key);
        // A session filter returns that conversation's failure as well as its
        // success, which is the point of naming the loop.
        assert!(Query::parse("session=fp:aa").matches(&stalled));
        assert!(Query::parse("session=fp:aa").matches(&recovered));
        assert!(Query::parse("loop=2").matches(&stalled));
        assert!(Query::parse("round>=3").matches(&stalled));
        // And it still does not leak into another conversation.
        assert!(!Query::parse("session=fp:bb").matches(&stalled));
    }

    #[test]
    fn loop_and_round_filters_select_within_a_loop() {
        let mut rec = log(None, "completion");
        rec.loop_key = Some(LoopKey {
            session_id: "fp:aa".to_string(),
            loop_seq: 3,
            round_index: 7,
        });
        assert!(Query::parse("loop=3").matches(&rec));
        assert!(!Query::parse("loop=4").matches(&rec));
        assert!(Query::parse("round>=7").matches(&rec));
        assert!(Query::parse("round>=6").matches(&rec));
        assert!(!Query::parse("round>=8").matches(&rec));
    }

    #[test]
    fn loop_level_filters_combine_with_a_mode_word() {
        let q = Query::parse("session=fp:aa loops");
        assert!(q.is_loops);
        assert_eq!(q.session_filter.as_deref(), Some("fp:aa"));
        let q = Query::parse("depth");
        assert!(q.is_depth);
    }

    #[test]
    fn matches_loop_selects_only_loop_records() {
        let q = Query::parse("depth");
        assert!(q.matches_loop(&loop_log("fp:aa", 1, 5, 5, 60_000)));
        // A request record is not a loop summary, whatever else it carries.
        assert!(!q.matches_loop(&log(None, "completion")));
        assert!(!q.matches_loop(&log(None, "ttfb")));
    }

    #[test]
    fn matches_loop_ignores_request_level_filters() {
        // A loop summary has no model, no status and no latency of its own, so
        // these filters must not decide whether it appears. If they did,
        // `model=... depth` would return zero loops and read as "no deep turns".
        let q = Query::parse("depth model=nothing-matches latency>999999 success=false error~nope");
        assert!(q.matches_loop(&loop_log("fp:aa", 1, 5, 5, 60_000)));
    }

    #[test]
    fn matches_loop_honours_session_and_loop_filters() {
        let summary = loop_log("fp:aa", 2, 5, 5, 60_000);
        assert!(Query::parse("depth session=fp:aa").matches_loop(&summary));
        assert!(!Query::parse("depth session=fp:bb").matches_loop(&summary));
        assert!(Query::parse("depth loop=2").matches_loop(&summary));
        assert!(!Query::parse("depth loop=3").matches_loop(&summary));
    }

    #[test]
    fn matches_loop_rejects_a_contradicting_record_kind() {
        let summary = loop_log("fp:aa", 1, 5, 5, 60_000);
        assert!(Query::parse("depth record_kind=loop").matches_loop(&summary));
        assert!(!Query::parse("depth record_kind=completion").matches_loop(&summary));
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let v = [1u64, 2, 3, 4];
        assert_eq!(percentile(&v, 50), 2);
        assert_eq!(percentile(&v, 90), 4);
        assert_eq!(percentile(&v, 99), 4);
        assert_eq!(percentile(&[], 50), 0, "an empty sample must not panic");
    }

    #[test]
    fn old_records_without_loop_fields_still_parse() {
        // The loop fields are additive: a line written before any of them
        // existed must still deserialize, with every loop field absent rather
        // than defaulted to a value that could be mistaken for a real zero.
        let raw = r#"{"timestamp":"2026-10-16T00:00:00Z","requested_model":"m","responding_model":"m","latency_ms":1,"success":true,"status_code":200,"has_tool_calls":false,"tool_call_count":0,"error":null,"is_racing":false,"record_kind":"completion"}"#;
        let parsed: TurnLog = serde_json::from_str(raw).unwrap();
        assert!(parsed.loop_key.is_none());
        assert_eq!(parsed.loop_rounds, None);
        assert_eq!(parsed.loop_wall_ms, None);
        assert_eq!(parsed.loop_tool_calls_distinct, None);
        assert_eq!(parsed.loop_ended_by, None);
    }

    #[test]
    fn a_loop_record_round_trips() {
        let summary = loop_log("fp:aa", 4, 56, 53, 2_142_000);
        let encoded = serde_json::to_string(&summary).unwrap();
        let decoded: TurnLog = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.loop_rounds, Some(56));
        assert_eq!(decoded.loop_completed_rounds, Some(53));
        assert_eq!(decoded.loop_wall_ms, Some(2_142_000));
        assert_eq!(decoded.loop_tool_calls_distinct, Some(168));
        assert_eq!(
            decoded.loop_key.as_ref().map(|k| k.session_id.as_str()),
            Some("fp:aa")
        );
    }
}
