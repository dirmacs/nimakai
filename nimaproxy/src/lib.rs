pub mod config;
pub mod key_pool;
pub mod model_router;
pub mod model_stats;
pub mod proxy;

pub use config::{load as config_load, Config, KeyEntry, RoutingConfig};
pub use key_pool::KeyPool;
pub use model_router::{ModelRouter, Strategy};
pub use model_stats::{ModelStatsStore, ModelSnapshot};
use reqwest::Client;
use std::os::raw::c_char;
use std::ptr;
use std::sync::Arc;

pub struct AppState {
    pub pool: KeyPool,
    pub client: Client,
    pub target: String,
    pub router: Option<ModelRouter>,
    pub model_stats: ModelStatsStore,
    pub racing_models: Vec<String>,
    pub racing_max_parallel: usize,
    pub racing_timeout_ms: u64,
    pub racing_strategy: String,
}

impl AppState {
    pub fn new(
        keys: Vec<KeyEntry>,
        target: String,
        router: Option<ModelRouter>,
        model_stats: ModelStatsStore,
        racing_models: Vec<String>,
        racing_max_parallel: usize,
        racing_timeout_ms: u64,
        racing_strategy: String,
    ) -> Arc<Self> {
        let client = Client::builder()
            .use_rustls_tls()
            .timeout(std::time::Duration::from_secs(120))
            .pool_max_idle_per_host(16)
            .build()
            .expect("failed to build HTTP client");

        Arc::new(AppState {
            pool: KeyPool::new(keys),
            client,
            target,
            router,
            model_stats,
            racing_models,
            racing_max_parallel,
            racing_timeout_ms,
            racing_strategy,
        })
    }
}

use std::ffi::{CStr, CString};

/// FFI: Start the proxy server by spawning a new process via posix_spawn.
/// config_path: absolute path to TOML config file
/// port:        TCP port to bind (overrides listen in config)
/// Returns: 0 on success, -1 on failure (already running, bad config, etc.)
#[no_mangle]
pub extern "C" fn proxy_start(config_path: *const c_char, port: u32) -> i32 {
    if config_path.is_null() {
        return -1;
    }
    let path = unsafe { CStr::from_ptr(config_path).to_str().unwrap_or("") };

    if let Ok(pid_str) = std::fs::read_to_string("/tmp/nimaproxy.pid") {
        if let Ok(pid) = pid_str.trim().parse::<libc::pid_t>() {
            if unsafe { libc::kill(pid, 0) } == 0 {
                return -1;
            }
        }
        std::fs::remove_file("/tmp/nimaproxy.pid").ok();
    }

    if let Err(e) = config_load(path) {
        std::eprintln!("[nimaproxy] config error: {}", e);
        return -1;
    }

    std::fs::write("/tmp/nimaproxy.pid", "starting").ok();

    let port_cstr   = CString::new(port.to_string()).unwrap();
    let config_cstr = CString::new(path).unwrap();
    let bin_path    = CString::new("/opt/nimakai/nimaproxy-bin").unwrap();
    let cf_flag     = CString::new("--config").unwrap();
    let pt_flag     = CString::new("--port").unwrap();

    let mut attrs:       libc::posix_spawnattr_t = unsafe { std::mem::zeroed() };
    let mut file_actions: libc::posix_spawn_file_actions_t = unsafe { std::mem::zeroed() };

    unsafe {
        libc::posix_spawnattr_init(&mut attrs);
        libc::posix_spawn_file_actions_init(&mut file_actions);
        libc::posix_spawnattr_setflags(&mut attrs, libc::POSIX_SPAWN_SETSID as libc::c_short);
        libc::posix_spawn_file_actions_addopen(
            &mut file_actions,
            libc::STDIN_FILENO,
            b"/dev/null\0".as_ptr() as *const c_char,
            libc::O_RDWR,
            0o644,
        );
        libc::posix_spawn_file_actions_adddup2(&mut file_actions, libc::STDIN_FILENO, libc::STDOUT_FILENO);
        libc::posix_spawn_file_actions_adddup2(&mut file_actions, libc::STDIN_FILENO, libc::STDERR_FILENO);
    }

    let mut child_pid: libc::pid_t = 0;
    let mut argv: [*mut c_char; 6] = [
        bin_path.as_ptr() as *mut c_char,
        cf_flag.as_ptr()  as *mut c_char,
        config_cstr.as_ptr() as *mut c_char,
        pt_flag.as_ptr()  as *mut c_char,
        port_cstr.as_ptr() as *mut c_char,
        ptr::null_mut(),
    ];

    let spawn_result = unsafe {
        libc::posix_spawn(
            &mut child_pid,
            bin_path.as_ptr(),
            &file_actions,
            &mut attrs,
            argv.as_mut_ptr(),
            ptr::null_mut(),
        )
    };

    unsafe {
        libc::posix_spawnattr_destroy(&mut attrs);
        libc::posix_spawn_file_actions_destroy(&mut file_actions);
    }

    if spawn_result != 0 {
        std::fs::remove_file("/tmp/nimaproxy.pid").ok();
        return -1;
    }

    std::thread::sleep(std::time::Duration::from_millis(500));

    let content = std::fs::read_to_string("/tmp/nimaproxy.pid").unwrap_or_default();
    if content.trim() == "starting" || content.trim().is_empty() {
        std::fs::remove_file("/tmp/nimaproxy.pid").ok();
        unsafe { libc::kill(child_pid, libc::SIGTERM); }
        return -1;
    }

    0
}

/// FFI: Stop the proxy server. Returns 0 on success, -1 if not running.
#[no_mangle]
pub extern "C" fn proxy_stop() -> i32 {
    let pid: libc::pid_t = std::fs::read_to_string("/tmp/nimaproxy.pid")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    if pid == 0 {
        return -1;
    }

    unsafe { libc::kill(pid, libc::SIGTERM); }
    std::fs::remove_file("/tmp/nimaproxy.pid").ok();
    0
}

/// FFI: Get health status. Returns JSON C string (caller must free with proxy_free_string).
#[no_mangle]
pub extern "C" fn proxy_health() -> *mut c_char {
    let pid: libc::pid_t = std::fs::read_to_string("/tmp/nimaproxy.pid")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    if pid == 0 || unsafe { libc::kill(pid, 0) } != 0 {
        std::fs::remove_file("/tmp/nimaproxy.pid").ok();
        return std::ptr::null_mut();
    }

    let body = reqwest::blocking::Client::new()
        .get("http://127.0.0.1:8080/health")
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .ok()
        .and_then(|r: reqwest::blocking::Response| r.text().ok());

    match body {
        Some(b) => CString::new(b).unwrap().into_raw(),
        None => std::ptr::null_mut(),
    }
}

/// FFI: Get per-model latency stats. Returns JSON C string (caller must free with proxy_free_string).
#[no_mangle]
pub extern "C" fn proxy_stats() -> *mut c_char {
    let pid: libc::pid_t = std::fs::read_to_string("/tmp/nimaproxy.pid")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    if pid == 0 || unsafe { libc::kill(pid, 0) } != 0 {
        std::fs::remove_file("/tmp/nimaproxy.pid").ok();
        return std::ptr::null_mut();
    }

    let body = reqwest::blocking::Client::new()
        .get("http://127.0.0.1:8080/stats")
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .ok()
        .and_then(|r: reqwest::blocking::Response| r.text().ok());

    match body {
        Some(b) => CString::new(b).unwrap().into_raw(),
        None => std::ptr::null_mut(),
    }
}

/// FFI: Free a C string returned by proxy_health or proxy_stats.
#[no_mangle]
pub extern "C" fn proxy_free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    unsafe { let _ = CString::from_raw(s); }
}
