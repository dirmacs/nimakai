//! Structured turn logging for observability and analysis.
//! Logs each request/response pair to JSONL format.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{self, Write, BufWriter};
use std::path::Path;
use std::sync::Mutex;

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
        
        let mut writer_guard = self.writer.lock().unwrap();
        
        // Reopen if needed (e.g., after rotation)
        if writer_guard.is_none() {
            self.open_file()?;
            writer_guard = self.writer.lock().unwrap();
        }
        
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
static mut GLOBAL_LOGGER: Option<TurnLogger> = None;

/// Initialize the global turn logger
pub fn init_logger(path: &str, enabled: bool) -> io::Result<()> {
    let logger = TurnLogger::new(path, enabled)?;
    unsafe {
        GLOBAL_LOGGER = Some(logger);
    }
    Ok(())
}

/// Log a turn to the global logger
pub fn log_turn(turn: &TurnLog) {
    unsafe {
        if let Some(logger) = &GLOBAL_LOGGER {
            let _ = logger.log(turn);
        }
    }
}

/// Get reference to global logger if enabled
pub fn with_logger<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&TurnLogger) -> R,
{
    unsafe {
        GLOBAL_LOGGER.as_ref().map(|logger| f(logger))
    }
}
