//! Structured turn logging for observability and analysis.
//! Logs request/response metadata to JSONL format. Message capture is available
//! in the data model, but the proxy path currently writes counts and status only.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// A single message in a conversation
#[derive(Serialize, Debug, Clone)]
pub struct MessageLog {
    pub role: String,
    pub content: String,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<String>, // JSON string for complex structure
}

/// Metadata about a completed turn (request/response pair)
#[derive(Serialize, Debug, Clone)]
pub struct TurnLog {
    /// ISO8601 timestamp
    pub timestamp: DateTime<Utc>,

    /// Request ID (UUID if provided, else generated)
    pub request_id: Option<String>,

    /// Model that was requested
    pub requested_model: String,

    /// Actual model that responded (after routing)
    pub responding_model: String,

    /// Time to first byte (ms)
    pub latency_ms: u64,

    /// Whether the request succeeded
    pub success: bool,

    /// HTTP status code
    pub status_code: u16,

    /// Number of messages in request
    pub request_message_count: usize,

    /// Number of messages in response  
    pub response_message_count: usize,

    /// Total tokens in request (if available)
    pub request_tokens: Option<u32>,

    /// Total tokens in response (if available)
    pub response_tokens: Option<u32>,

    /// Whether tool calls were present
    pub has_tool_calls: bool,

    /// Tool call count
    pub tool_call_count: usize,

    /// Error message if failed
    pub error: Option<String>,

    /// Key label used (if available)
    pub key_label: Option<String>,

    /// Whether this was a racing request
    pub is_racing: bool,

    /// Racing context: how many models were raced
    pub racing_models_count: Option<usize>,

    /// Racing context: did this model win the race?
    pub racing_winner: Option<bool>,

    /// Full request messages (redacted if needed)
    pub request_messages: Vec<MessageLog>,

    /// Response message (first choice only)
    pub response_message: Option<MessageLog>,
}

impl TurnLog {
    pub fn new(
        requested_model: String,
        responding_model: String,
        latency_ms: u64,
        success: bool,
        status_code: u16,
        request_message_count: usize,
        response_message_count: usize,
        has_tool_calls: bool,
        tool_call_count: usize,
        key_label: Option<String>,
        is_racing: bool,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            request_id: None,
            requested_model,
            responding_model,
            latency_ms,
            success,
            status_code,
            request_message_count,
            response_message_count,
            request_tokens: None,
            response_tokens: None,
            has_tool_calls,
            tool_call_count,
            error: None,
            key_label,
            is_racing,
            racing_models_count: None,
            racing_winner: None,
            request_messages: Vec::new(),
            response_message: None,
        }
    }
}

/// Turn logger with file output
pub struct TurnLogger {
    writer: Mutex<Option<BufWriter<File>>>,
    path: String,
    enabled: bool,
}

impl TurnLogger {
    pub fn new(path: &str, enabled: bool) -> io::Result<Self> {
        let logger = Self {
            writer: Mutex::new(None),
            path: path.to_string(),
            enabled,
        };

        if enabled {
            logger.open_file()?;
        }

        Ok(logger)
    }

    fn open_file(&self) -> io::Result<()> {
        // Create parent directory if needed
        if let Some(parent) = Path::new(&self.path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;

        *self.writer.lock().unwrap() = Some(BufWriter::new(file));
        Ok(())
    }

    pub fn log(&self, turn: &TurnLog) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }

        // The writer mutex must NOT be held while calling `open_file`.
        //
        // Holding it used to deadlock: `log` locked `self.writer`, found it
        // `None` (e.g. after `rotate`), and called `open_file` which locks
        // `self.writer` again — `std::sync::Mutex` is not reentrant, so the
        // second lock blocked forever on the first one's guard. The re-lock on
        // the line after was unreachable.
        //
        // The consequence was not a crash but a wedged thread: after any
        // rotation the next `log` call hung the caller instead of writing, so
        // turn logging stopped with no error at all — the same silent shape as
        // a permission failure.
        let needs_open = {
            let guard = self.writer.lock().unwrap();
            guard.is_none()
        };
        if needs_open {
            self.open_file()?;
        }

        let mut writer_guard = self.writer.lock().unwrap();
        if let Some(writer) = writer_guard.as_mut() {
            serde_json::to_writer(&mut *writer, turn)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }

        Ok(())
    }

    pub fn rotate(&self) -> io::Result<()> {
        // Close current file
        {
            let mut writer_guard = self.writer.lock().unwrap();
            *writer_guard = None;
        }

        // Rotate file (add timestamp suffix)
        let path = Path::new(&self.path);
        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        let rotated_path = format!("{}.{}", path.display(), timestamp);

        if path.exists() {
            std::fs::rename(path, &rotated_path)?;
        }

        // Open fresh file
        self.open_file()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Global turn logger instance
static GLOBAL_LOGGER: OnceLock<TurnLogger> = OnceLock::new();

/// Initialize the global turn logger
pub fn init_logger(path: &str, enabled: bool) -> io::Result<()> {
    let logger = TurnLogger::new(path, enabled)?;
    GLOBAL_LOGGER.set(logger).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "turn logger is already initialized",
        )
    })
}

/// Log a turn to the global logger
pub fn log_turn(turn: &TurnLog) {
    if let Some(logger) = GLOBAL_LOGGER.get() {
        let _ = logger.log(turn);
    }
}

/// Get reference to global logger if enabled
pub fn with_logger<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&TurnLogger) -> R,
{
    GLOBAL_LOGGER.get().map(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_turn_log_creation() {
        let turn = TurnLog::new(
            "auto".to_string(),
            "mistralai/mistral-medium-3.5-128b".to_string(),
            742,
            true,
            200,
            5,
            1,
            false,
            0,
            Some("test".to_string()),
            true,
        );

        assert_eq!(turn.requested_model, "auto");
        assert_eq!(turn.responding_model, "mistralai/mistral-medium-3.5-128b");
        assert_eq!(turn.latency_ms, 742);
        assert!(turn.success);
        assert!(!turn.has_tool_calls);
    }

    #[test]
    fn test_turn_log_with_messages() {
        let mut turn = TurnLog::new(
            "auto".to_string(),
            "qwen/qwen3.5".to_string(),
            891,
            true,
            200,
            3,
            1,
            true,
            2,
            None,
            false,
        );

        turn.request_messages = vec![
            MessageLog {
                role: "user".to_string(),
                content: "Hello".to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
            MessageLog {
                role: "assistant".to_string(),
                content: "Hi there!".to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        turn.response_message = Some(MessageLog {
            role: "assistant".to_string(),
            content: "How can I help?".to_string(),
            tool_call_id: None,
            tool_calls: None,
        });

        assert_eq!(turn.request_messages.len(), 2);
        assert_eq!(turn.request_messages[0].role, "user");
        assert!(turn.response_message.is_some());
    }

    #[test]
    fn test_logger_initially_disabled() {
        let logger = TurnLogger::new("/tmp/test_turns.jsonl", false).unwrap();
        assert!(!logger.is_enabled());
    }

    #[test]
    fn test_turn_log_serialization() {
        let turn = TurnLog::new(
            "auto".to_string(),
            "test-model".to_string(),
            100,
            true,
            200,
            2,
            1,
            false,
            0,
            Some("key1".to_string()),
            false,
        );

        let json = serde_json::to_string(&turn).unwrap();
        assert!(json.contains("test-model"));
        assert!(json.contains("requested_model"));
    }

    /// The write path itself, previously 0% covered.
    ///
    /// Turn logging is the operator's only view of what the proxy did, and it
    /// has been silently off in the past (permission errors on the container
    /// user). These tests pin the behaviour that must hold: an enabled logger
    /// writes a complete JSON line, a disabled logger writes nothing at all,
    /// and a logger whose writer was rotated away reopens and continues.
    fn sample_turn() -> TurnLog {
        TurnLog::new(
            "auto".to_string(),
            "test/model".to_string(),
            1234,
            true,
            200,
            3,
            1,
            false,
            0,
            Some("key-a".to_string()),
            false,
        )
    }

    #[test]
    fn test_enabled_logger_writes_one_json_line_per_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        let logger = TurnLogger::new(path.to_str().unwrap(), true).unwrap();

        logger.log(&sample_turn()).unwrap();
        logger.log(&sample_turn()).unwrap();
        drop(logger);

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "one line per record, got {lines:?}");
        // Every line must be a complete, parseable JSON object.
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["requested_model"], "auto");
            assert_eq!(v["status_code"], 200);
        }
    }

    #[test]
    fn test_disabled_logger_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        let logger = TurnLogger::new(path.to_str().unwrap(), false).unwrap();

        // Must succeed silently rather than erroring, and must not create a file.
        logger.log(&sample_turn()).unwrap();
        assert!(
            !path.exists(),
            "a disabled logger must not create any file, found {:?}",
            path
        );
    }

    #[test]
    fn test_logger_reopens_after_writer_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        let logger = TurnLogger::new(path.to_str().unwrap(), true).unwrap();
        logger.log(&sample_turn()).unwrap();

        // Simulate rotation tearing down the writer: the next log must reopen
        // rather than silently dropping the record.
        *logger.writer.lock().unwrap() = None;
        logger.log(&sample_turn()).unwrap();
        drop(logger);

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content.lines().count(),
            2,
            "record after reopen must be persisted, not lost"
        );
    }

    #[test]
    fn test_logger_appends_never_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        // Pre-existing content must survive.
        std::fs::write(&path, b"existing line\n").unwrap();

        let logger = TurnLogger::new(path.to_str().unwrap(), true).unwrap();
        logger.log(&sample_turn()).unwrap();
        drop(logger);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.starts_with("existing line\n"),
            "append mode must preserve prior content, got {content:?}"
        );
        assert_eq!(content.lines().count(), 2);
    }

    #[test]
    fn test_logger_survives_unwritable_directory_and_reports_it() {
        // A logger pointed at an impossible path must FAIL LOUDLY rather than
        // silently disabling itself — the failure mode that hid a 12-day
        // logging outage in the deployed container.
        let bad = "/proc/definitely/not/writable/turns.jsonl";
        let result = TurnLogger::new(bad, true);
        assert!(
            result.is_err(),
            "an unwritable log path must return an error, not a silent no-op"
        );
    }
}
