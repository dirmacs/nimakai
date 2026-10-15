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

/// Current size of `path`, or 0 if it cannot be determined.
///
/// A rotation decision must not fail because of this: the worst case is that
/// the counter starts low and the first file lives a little past its
/// threshold, which is a size overrun, not lost records.
fn path_len(path: &str) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
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
    /// Bytes written to the current file since it was opened, and the
    /// threshold that triggers a rotation.
    ///
    /// Both live behind the writer mutex so the check and the write see the
    /// same value: a rotation decided from a stale byte count can be lost
    /// between concurrent `log` calls. The counter is reset by `rotate`,
    /// which also swaps in a fresh file, so the two can never disagree.
    bytes_since_rotate: Mutex<u64>,
    rotate_at_bytes: u64,
    /// Logged once, the first time a rotation succeeds, so an operator can
    /// see that rotation is live rather than inferring it from filenames.
    rotated_once: std::sync::atomic::AtomicBool,
}

impl TurnLogger {
    /// Default threshold: 64 MiB.
    ///
    /// The turn log is the operator's only record of what the proxy did, so
    /// rotation must not discard anything that has not been accounted for —
    /// it moves the file aside, it never truncates. A large threshold keeps
    /// the number of rotated files (and therefore inode churn) low on a
    /// busy host while still bounding disk use.
    pub const DEFAULT_ROTATE_AT_BYTES: u64 = 64 * 1024 * 1024;

    pub fn new(path: &str, enabled: bool) -> io::Result<Self> {
        Self::with_rotate_at(path, enabled, Self::DEFAULT_ROTATE_AT_BYTES)
    }

    /// Build a logger that rotates once the current file reaches
    /// `rotate_at_bytes`. `rotate_at_bytes == 0` disables rotation.
    pub fn with_rotate_at(path: &str, enabled: bool, rotate_at_bytes: u64) -> io::Result<Self> {
        let logger = Self {
            writer: Mutex::new(None),
            path: path.to_string(),
            enabled,
            bytes_since_rotate: Mutex::new(0),
            rotate_at_bytes,
            rotated_once: std::sync::atomic::AtomicBool::new(false),
        };

        if enabled {
            logger.open_file()?;
        }

        Ok(logger)
    }

    /// Bytes written to the current file so far.
    pub fn bytes_since_rotate(&self) -> u64 {
        *self.bytes_since_rotate.lock().unwrap()
    }

    /// Whether this logger rotates, and at what size.
    pub fn rotate_threshold(&self) -> Option<u64> {
        (self.rotate_at_bytes > 0).then_some(self.rotate_at_bytes)
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

        // Seed the counter from the file's existing size so a restart does not
        // postpone rotation by a whole threshold: the log is append-only across
        // restarts, so the file on disk already holds everything the counter
        // would otherwise consider unseen.
        let existing = path_len(&self.path).unwrap_or(0);
        *self.bytes_since_rotate.lock().unwrap() = existing;

        *self.writer.lock().unwrap() = Some(BufWriter::new(file));
        Ok(())
    }

    pub fn log(&self, turn: &TurnLog) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }

        // Serialize first so the byte count is exact rather than estimated:
        // the counter is what decides rotation, so it must match what the
        // file actually contains.
        let mut buf = Vec::new();
        {
            serde_json::to_writer(&mut buf, turn)?;
            buf.push(b'\n');
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
            writer.write_all(&buf)?;
            writer.flush()?;
        }

        // Rotate once this file is over the threshold. The check is after the
        // write (not before) so the file that triggered it keeps the record
        // that pushed it over rather than losing it on the next call.
        //
        // `rotate` swaps the writer and renames the file, and it is called
        // without the writer guarded here — it takes the lock itself. This is
        // the same non-reentrancy discipline as `open_file` above: holding the
        // writer across `rotate` would deadlock.
        if self.rotate_at_bytes > 0 {
            let mut bytes = self.bytes_since_rotate.lock().unwrap();
            *bytes += buf.len() as u64;
            if *bytes >= self.rotate_at_bytes {
                drop(bytes);
                drop(writer_guard);
                self.rotate()?;
            }
        }

        Ok(())
    }

    pub fn rotate(&self) -> io::Result<()> {
        // Close current file
        {
            let mut writer_guard = self.writer.lock().unwrap();
            *writer_guard = None;
        }

        // Move the current file aside, taking care not to clobber a file that
        // is already there.
        //
        // A plain `rename` onto a second-granularity timestamp silently
        // destroys the earlier file whenever two rotations land in the same
        // second — which is exactly what happens on a fast sequence of small
        // rotations. The turn log is the operator's only record of what the
        // proxy did, so "deleted the previous rotation's records" is not an
        // acceptable cost. Instead, walk a counter until an unused name is
        // found; the first rotation of a second still gets the clean name.
        let path = Path::new(&self.path);
        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        if path.exists() {
            let base = format!("{}.{}", path.display(), timestamp);
            let mut candidate = base.clone();
            let mut n = 1u32;
            while Path::new(&candidate).exists() {
                candidate = format!("{base}.{n}");
                n += 1;
            }
            std::fs::rename(path, &candidate)?;
        }

        // Reset the counter: the file that just moved aside is accounted for
        // separately from now on. Without this the fresh file inherits the
        // old file's byte count and rotates again on the very next record.
        *self.bytes_since_rotate.lock().unwrap() = 0;

        // Open fresh file. `open_file` re-seeds the counter from the new
        // file's size, which is 0 because the rotation just moved the old
        // content aside — so the reset above and the seed agree.
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

    /// Every `turns.jsonl*` file in `dir`, with its line count.
    ///
    /// This is the assertion surface for rotation: the invariant is that the
    /// records are still somewhere in the directory, not in which file.
    fn records_in_dir(dir: &Path) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("turns") {
                continue;
            }
            let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
            out.push((name, content.lines().count()));
        }
        out.sort();
        out
    }

    #[test]
    fn test_rotation_fires_at_threshold_and_preserves_every_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");

        // A tiny threshold so a handful of records crosses it several times.
        let logger = TurnLogger::with_rotate_at(path.to_str().unwrap(), true, 600).unwrap();

        const RECORDS: usize = 25;
        for _ in 0..RECORDS {
            logger.log(&sample_turn()).unwrap();
        }
        drop(logger);

        let files = records_in_dir(dir.path());
        // One rotated file per crossing, plus the current live file.
        assert!(
            files.len() > 1,
            "a 25-record run at a 600-byte threshold must rotate, got {files:?}"
        );

        let total: usize = files.iter().map(|(_, n)| n).sum();
        assert_eq!(
            total, RECORDS,
            "rotation moves files aside, it must never drop a record: {files:?}"
        );

        // Every surviving line must still be a complete, parseable record.
        for (name, _) in &files {
            let content = std::fs::read_to_string(dir.path().join(name)).unwrap();
            for line in content.lines() {
                let v: serde_json::Value = serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("{name} has a corrupt line after rotation: {e}"));
                assert_eq!(v["requested_model"], "auto");
            }
        }
    }

    #[test]
    fn test_record_that_crosses_threshold_is_not_lost() {
        // The boundary case: the write that pushes the file over the limit
        // must land in the file it belongs to, not be deferred into the next
        // one and dropped.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");

        let one = serde_json::to_vec(&sample_turn()).unwrap().len() as u64 + 1;
        // Threshold exactly one record wide: every log call rotates.
        let logger = TurnLogger::with_rotate_at(path.to_str().unwrap(), true, one).unwrap();
        const RECORDS: usize = 3;
        for _ in 0..RECORDS {
            logger.log(&sample_turn()).unwrap();
        }
        drop(logger);

        let total: usize = records_in_dir(dir.path()).iter().map(|(_, n)| n).sum();
        assert_eq!(
            total, RECORDS,
            "the record that crosses the threshold must be written, not lost"
        );
    }

    #[test]
    fn test_rotation_can_be_disabled() {
        // `rotate_at_bytes == 0` must mean "never rotate", not "rotate
        // immediately" (which would be the reading if 0 were compared
        // directly against the byte counter, since 0 >= 0).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");

        let logger = TurnLogger::with_rotate_at(path.to_str().unwrap(), true, 0).unwrap();
        for _ in 0..25 {
            logger.log(&sample_turn()).unwrap();
        }
        assert_eq!(logger.rotate_threshold(), None);
        drop(logger);

        let files = records_in_dir(dir.path());
        assert_eq!(
            files,
            vec![("turns.jsonl".to_string(), 25)],
            "rotation disabled must keep every record in the live file, got {files:?}"
        );
    }

    #[test]
    fn test_preexisting_log_does_not_postpone_rotation() {
        // A restart into a log that is already over the threshold must rotate
        // on the first record rather than granting a fresh threshold. The file
        // is append-only across restarts, so its size is the honest starting
        // byte count.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");

        // 800 bytes of pre-existing content, threshold 600.
        std::fs::write(&path, vec![b'x'; 800]).unwrap();
        let logger = TurnLogger::with_rotate_at(path.to_str().unwrap(), true, 600).unwrap();
        assert!(
            logger.bytes_since_rotate() >= 600,
            "the existing file's size must seed the counter, got {}",
            logger.bytes_since_rotate()
        );

        logger.log(&sample_turn()).unwrap();
        drop(logger);

        // The pre-existing content must have been moved aside, not overwritten.
        let files = records_in_dir(dir.path());
        let rotated = files.iter().find(|(n, _)| n != "turns.jsonl");
        assert!(
            rotated.is_some(),
            "an over-threshold file must rotate on first write, got {files:?}"
        );
        let moved = std::fs::read_to_string(dir.path().join(&rotated.unwrap().0)).unwrap();
        // The rotated file holds the original 800 bytes plus the record that
        // triggered the rotation: `log` writes first and rotates after, so the
        // record lands in the file it was written to rather than being lost.
        let original = &moved[..800];
        assert!(
            original.chars().all(|c| c == 'x'),
            "rotation must move the existing content aside intact, got {original:?}"
        );
        assert!(
            moved.contains("requested_model"),
            "the triggering record must be in the rotated file too, got {moved:?}"
        );
    }
}
