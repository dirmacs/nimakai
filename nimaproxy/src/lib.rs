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
use std::sync::{Arc, Mutex};

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
    pub racing_cursor: Mutex<usize>,
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
            racing_cursor: Mutex::new(0),
        })
    }
}

use std::ffi::{CStr, CString};
use std::path::PathBuf;

fn pid_file_path() -> PathBuf {
    std::env::var("NIMAPROXY_PID_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/nimaproxy.pid"))
}

/// FFI: Start the proxy server by spawning a new process via posix_spawn.
/// config_path: absolute path to TOML config file
/// port:        TCP port to bind (overrides listen in config)
/// Returns: 0 on success, -1 on failure (already running, bad config, etc.)
#[no_mangle]
pub extern "C" fn proxy_start(config_path: *const c_char, port: u32) -> i32 {
    if config_path.is_null() {
        std::eprintln!("[nimaproxy] proxy_start: null config");
        return -1;
    }
    let path = unsafe { CStr::from_ptr(config_path).to_str().unwrap_or("") };
    let pfile = pid_file_path();
    std::eprintln!("[nimaproxy] proxy_start: pid_file={:?}", pfile);

    if let Ok(pid_str) = std::fs::read_to_string(&pfile) {
        std::eprintln!("[nimaproxy] proxy_start: existing pid_file={:?}", pid_str.trim());
        if let Ok(pid) = pid_str.trim().parse::<libc::pid_t>() {
            if unsafe { libc::kill(pid, 0) } == 0 {
                std::eprintln!("[nimaproxy] proxy_start: already running pid={}", pid);
                return -1;
            }
        }
        std::fs::remove_file(&pfile).ok();
    }

    if let Err(e) = config_load(path) {
        std::eprintln!("[nimaproxy] proxy_start: config error: {}", e);
        return -1;
    }

    std::fs::write(&pfile, "starting").ok();

    let port_cstr    = CString::new(port.to_string()).unwrap();
    let config_cstr  = CString::new(path).unwrap();
    let bin_path     = CString::new("/opt/nimakai/nimaproxy-bin").unwrap();
    let cf_flag      = CString::new("--config").unwrap();
    let pt_flag      = CString::new("--port").unwrap();
    let pid_flag     = CString::new("--pid-file").unwrap();
    let pid_cstr     = CString::new(pfile.to_str().unwrap_or_default()).unwrap();

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
    let mut argv: Vec<*mut c_char> = vec![
        bin_path.as_ptr() as *mut c_char,
        cf_flag.as_ptr()  as *mut c_char,
        config_cstr.as_ptr() as *mut c_char,
        pt_flag.as_ptr()  as *mut c_char,
        port_cstr.as_ptr() as *mut c_char,
        pid_flag.as_ptr() as *mut c_char,
        pid_cstr.as_ptr() as *mut c_char,
    ];
    argv.push(ptr::null_mut());

    let spawn_result = unsafe {
        libc::posix_spawn(
            &mut child_pid,
            bin_path.as_ptr(),
            &file_actions,
            &mut attrs,
            argv.as_mut_ptr(),
            ptr::null(),
        )
    };

    unsafe {
        libc::posix_spawnattr_destroy(&mut attrs);
        libc::posix_spawn_file_actions_destroy(&mut file_actions);
    }

    if spawn_result != 0 {
        std::eprintln!(
            "[nimaproxy] proxy_start: spawn failed errno={} path={}",
            spawn_result,
            bin_path.to_str().unwrap_or("?")
        );
        std::fs::remove_file(&pfile).ok();
        return -1;
    }

    std::eprintln!("[nimaproxy] proxy_start: spawned pid={}", child_pid);
    std::thread::sleep(std::time::Duration::from_millis(500));

    let content = std::fs::read_to_string(&pfile).unwrap_or_default();
    std::eprintln!("[nimaproxy] proxy_start: pid_file content after wait={:?}", content.trim());
    if content.trim() == "starting" || content.trim().is_empty() {
        std::fs::remove_file(&pfile).ok();
        unsafe { libc::kill(child_pid, libc::SIGTERM); }
        std::eprintln!("[nimaproxy] proxy_start: proxy failed to write PID (still 'starting')");
        return -1;
    }

    0
}

/// FFI: Stop the proxy server. Returns 0 on success (including if already stopped), -1 on error.
#[no_mangle]
pub extern "C" fn proxy_stop() -> i32 {
    let pid: libc::pid_t = std::fs::read_to_string(pid_file_path())
        .ok()
        .and_then(|s| s.trim().split(':').next().and_then(|p| p.parse().ok()))
        .unwrap_or(0);

    if pid == 0 {
        return 0;
    }

    unsafe { libc::kill(pid, libc::SIGTERM); }
    std::fs::remove_file(pid_file_path()).ok();
    0
}

/// FFI: Get health status. Returns JSON C string (caller must free with proxy_free_string).
#[no_mangle]
pub extern "C" fn proxy_health() -> *mut c_char {
    let port: u16 = std::fs::read_to_string(pid_file_path())
        .ok()
        .and_then(|s| s.trim().split(':').nth(1).and_then(|p| p.parse().ok()))
        .unwrap_or(8080);

    let pid: libc::pid_t = std::fs::read_to_string(pid_file_path())
        .ok()
        .and_then(|s| s.trim().split(':').next().and_then(|p| p.parse().ok()))
        .unwrap_or(0);

    if pid == 0 || unsafe { libc::kill(pid, 0) } != 0 {
        std::fs::remove_file(pid_file_path()).ok();
        return std::ptr::null_mut();
    }

    let body = reqwest::blocking::Client::new()
        .get(format!("http://127.0.0.1:{}/health", port))
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
    let port: u16 = std::fs::read_to_string(pid_file_path())
        .ok()
        .and_then(|s| s.trim().split(':').nth(1).and_then(|p| p.parse().ok()))
        .unwrap_or(8080);

    let pid: libc::pid_t = std::fs::read_to_string(pid_file_path())
        .ok()
        .and_then(|s| s.trim().split(':').next().and_then(|p| p.parse().ok()))
        .unwrap_or(0);

    if pid == 0 || unsafe { libc::kill(pid, 0) } != 0 {
        std::fs::remove_file(pid_file_path()).ok();
        return std::ptr::null_mut();
    }

    let body = reqwest::blocking::Client::new()
        .get(format!("http://127.0.0.1:{}/stats", port))
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

#[cfg(test)]
mod ffi_tests {
    use super::*;
    use std::ffi::CString;
    use tempfile::TempDir;

    const NVIDIA_API_KEY: &str = "REDACTED_KEY_1";

    fn with_isolated_env<T>(pid: u16, f: impl FnOnce(&str, &str) -> T) -> T {
        let dir = TempDir::new().expect("temp dir");
        let pid_file = dir.path().join("nimaproxy.pid");
        let config_file = dir.path().join("nimaproxy.toml");

        let config = format!(
            r#"listen = "127.0.0.1:{}"
[[keys]]
key = "{}"
label = "test"
"#,
            pid, NVIDIA_API_KEY
        );
        std::fs::write(&config_file, config).expect("write config");

        std::env::set_var("NIMAPROXY_PID_FILE", &pid_file);

        let result = f(config_file.to_str().unwrap(), pid_file.to_str().unwrap());

        std::env::remove_var("NIMAPROXY_PID_FILE");
        drop(dir);
        result
    }

    #[test]
    fn test_proxy_start_stop_cycle() {
        with_isolated_env(19101, |cfg_path, pid_file| {
            let config_path = CString::new(cfg_path).unwrap();
            let result = unsafe { proxy_start(config_path.as_ptr(), 0) };
            assert_eq!(result, 0, "proxy_start should succeed");

            std::thread::sleep(std::time::Duration::from_millis(500));
            let pid_content = std::fs::read_to_string(pid_file).unwrap_or_default();
            assert!(!pid_content.is_empty() && pid_content != "starting", "pid file should be written");

            let stop_result = unsafe { proxy_stop() };
            assert_eq!(stop_result, 0, "proxy_stop should succeed");
        });
    }

    #[test]
    fn test_proxy_health_when_running() {
        with_isolated_env(19102, |cfg_path, _pid_file| {
            let config_path = CString::new(cfg_path).unwrap();
            unsafe { proxy_start(config_path.as_ptr(), 0) };
            std::thread::sleep(std::time::Duration::from_millis(600));

            let health = unsafe { proxy_health() };
            assert!(!health.is_null(), "health should return valid string when running");

            unsafe { proxy_free_string(health) };
            unsafe { proxy_stop() };
        });
    }

    #[test]
    fn test_proxy_stats_when_running() {
        with_isolated_env(19103, |cfg_path, _pid_file| {
            let config_path = CString::new(cfg_path).unwrap();
            unsafe { proxy_start(config_path.as_ptr(), 0) };
            std::thread::sleep(std::time::Duration::from_millis(600));

            let stats = unsafe { proxy_stats() };
            assert!(!stats.is_null(), "stats should return valid string when running");

            unsafe { proxy_free_string(stats) };
            unsafe { proxy_stop() };
        });
    }

    #[test]
    fn test_proxy_health_when_stopped() {
        let health = unsafe { proxy_health() };
        assert!(health.is_null(), "health should return null when not running");
    }

    #[test]
    fn test_proxy_stop_idempotent() {
        let result1 = unsafe { proxy_stop() };
        let result2 = unsafe { proxy_stop() };

        assert_eq!(result1, 0, "first stop should return 0");
        assert_eq!(result2, 0, "second stop should also return 0 (idempotent)");
    }

    #[test]
    fn test_proxy_start_already_running() {
        with_isolated_env(19104, |cfg_path, _pid_file| {
            let config_path = CString::new(cfg_path).unwrap();
            let result1 = unsafe { proxy_start(config_path.as_ptr(), 0) };
            assert_eq!(result1, 0, "first start should succeed");

            std::thread::sleep(std::time::Duration::from_millis(600));

            let result2 = unsafe { proxy_start(config_path.as_ptr(), 0) };
            assert_eq!(result2, -1, "second start should fail (already running)");

            unsafe { proxy_stop() };
        });
    }

    #[test]
    fn test_proxy_start_invalid_config() {
        with_isolated_env(19105, |_cfg_path, _pid_file| {
            let config_path = CString::new("/nonexistent/config.toml").unwrap();
            let result = unsafe { proxy_start(config_path.as_ptr(), 0) };
            assert_eq!(result, -1, "start with invalid config should fail");
        });
    }

    #[test]
    fn test_proxy_start_with_custom_port() {
        with_isolated_env(19106, |cfg_path, pid_file| {
            let config_path = CString::new(cfg_path).unwrap();
            let result = unsafe { proxy_start(config_path.as_ptr(), 19106) };
            assert_eq!(result, 0, "proxy_start with custom port should succeed");

            std::thread::sleep(std::time::Duration::from_millis(600));

            let pid_content = std::fs::read_to_string(pid_file).unwrap_or_default();
            assert!(pid_content.contains("19106"), "PID file should contain custom port {}", pid_content);

            unsafe { proxy_stop() };
        });
    }
}
