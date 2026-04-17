pub mod config;
pub mod key_pool;
pub mod model_router;
pub mod model_stats;
pub mod proxy;

pub use proxy::validate_model_exists;

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
    pub available_models: Mutex<Vec<String>>,
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

        let available_models = racing_models.clone();
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
            available_models: Mutex::new(available_models),
        })
    }
}

use std::ffi::{CStr, CString};
use std::fs;
use std::path::PathBuf;
use std::cell::RefCell;

thread_local! {
    static TLS_PID_FILE: RefCell<Option<PathBuf>> = RefCell::new(None);
}

fn set_tls_pid_file(path: &str) {
    TLS_PID_FILE.with(|tls| {
        *tls.borrow_mut() = Some(PathBuf::from(path));
    });
}

fn pid_file_path(override_path: Option<&str>) -> PathBuf {
    if let Some(p) = override_path {
        return PathBuf::from(p);
    }
    let tls_path = TLS_PID_FILE.with(|tls| tls.borrow().clone());
    if let Some(p) = tls_path {
        return p;
    }
    std::env::var("NIMAPROXY_PID_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/nimaproxy.pid"))
}

fn is_process_alive(pid: libc::pid_t) -> bool {
    let result: i32 = unsafe { libc::kill(pid, 0) };
    result == 0
}

fn read_pid_and_port(pfile: &PathBuf) -> Option<(libc::pid_t, u16)> {
    let content = std::fs::read_to_string(pfile).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() || trimmed == "starting" {
        return None;
    }
    let parts: Vec<&str> = trimmed.split(':').collect();
    let pid = parts.first()?.parse::<libc::pid_t>().ok()?;
    let port = parts.get(1).and_then(|p| p.parse::<u16>().ok()).unwrap_or(8080);
    Some((pid, port))
}

fn check_proxy_alive(port: u16) -> bool {
    if let Ok(resp) = reqwest::blocking::Client::new()
        .get(format!("http://127.0.0.1:{}/health", port))
        .timeout(std::time::Duration::from_millis(200))
        .send()
    {
        resp.status().is_success() || resp.status().as_u16() == 200
    } else {
        false
    }
}

fn wait_for_proxy_ready(port: u16, timeout_ms: u64) -> bool {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        if let Ok(resp) = client
            .get(format!("http://127.0.0.1:{}/health", port))
            .send()
        {
            if resp.status().is_success() || resp.status().as_u16() == 200 {
                return true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

#[no_mangle]
pub extern "C" fn proxy_start(config_path: *const c_char, port: u32) -> i32 {
    proxy_start_with_pid_file(config_path, port, std::ptr::null())
}

/// FFI: Start the proxy server with explicit PID file path (optional override).
/// If pid_file is provided as a C string, it takes precedence over NIMAPROXY_PID_FILE env var.
#[no_mangle]
pub extern "C" fn proxy_start_with_pid_file(
    config_path: *const c_char,
    port: u32,
    pid_file: *const c_char,
) -> i32 {
    let pfile = pid_file_path(if pid_file.is_null() {
        None
    } else {
        let path = unsafe { CStr::from_ptr(pid_file).to_str().unwrap_or("") };
        Some(path)
    });
    std::eprintln!("[nimaproxy] proxy_start: pid_file={:?}", pfile);

    if let Some((existing_pid, existing_port)) = read_pid_and_port(&pfile) {
        std::eprintln!("[nimaproxy] proxy_start: existing pid={}, port={}", existing_pid, existing_port);
        if is_process_alive(existing_pid) && check_proxy_alive(existing_port) {
            std::eprintln!("[nimaproxy] proxy_start: already running pid={}, port={}", existing_pid, existing_port);
            return -1;
        }
    }

    if config_path.is_null() {
        std::eprintln!("[nimaproxy] proxy_start: null config");
        return -1;
    }
    let path = unsafe { CStr::from_ptr(config_path).to_str().unwrap_or("") };

    if let Err(e) = config_load(path) {
        std::eprintln!("[nimaproxy] proxy_start: config error: {}", e);
        return -1;
    }

    if let Err(e) = fs::write(&pfile, "starting") {
        std::eprintln!("[nimaproxy] proxy_start: failed to write pid file: {}", e);
        return -1;
    }

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

    let env_array: Vec<(String, String)> = std::env::vars().collect();
    let envp: Vec<*mut c_char> = env_array
        .iter()
        .map(|(k, v)| {
            CString::new(format!("{}={}", k, v))
                .expect("env var should be valid C string")
                .into_raw()
        })
        .chain(std::iter::once(ptr::null_mut()))
        .collect();

    let spawn_result = unsafe {
        libc::posix_spawn(
            &mut child_pid,
            bin_path.as_ptr(),
            &file_actions,
            &mut attrs,
            argv.as_mut_ptr(),
            envp.as_ptr(),
        )
    };

    for env_str in envp.iter().take(envp.len() - 1) {
        if !env_str.is_null() {
            unsafe { let _ = CString::from_raw(*env_str); }
        }
    }

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
        fs::remove_file(&pfile).ok();
        return -1;
    }

    std::eprintln!("[nimaproxy] proxy_start: spawned pid={}", child_pid);

    let start = std::time::Instant::now();
    let max_wait_ms = 5000u64;
    while start.elapsed().as_millis() < max_wait_ms as u128 {
        if let Some((written_pid, written_port)) = read_pid_and_port(&pfile) {
            if written_pid != child_pid {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            if wait_for_proxy_ready(written_port, 500) {
                std::eprintln!("[nimaproxy] proxy_start: proxy ready on port={}", written_port);
                return 0;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    std::eprintln!("[nimaproxy] proxy_start: proxy failed to become ready");
    fs::remove_file(&pfile).ok();
    unsafe { libc::kill(child_pid, libc::SIGTERM); }
    -1
}

/// FFI: Stop the proxy server. Returns 0 on success (including if already stopped), -1 on error.
#[no_mangle]
pub extern "C" fn proxy_stop() -> i32 {
    let pid: libc::pid_t = std::fs::read_to_string(pid_file_path(None))
        .ok()
        .and_then(|s| s.trim().split(':').next().and_then(|p| p.parse().ok()))
        .unwrap_or(0);

    if pid == 0 {
        return 0;
    }

    unsafe { libc::kill(pid, libc::SIGTERM); }
    std::fs::remove_file(pid_file_path(None)).ok();
    0
}

/// FFI: Get health status. Returns JSON C string (caller must free with proxy_free_string).
#[no_mangle]
pub extern "C" fn proxy_health() -> *mut c_char {
    let pfile = pid_file_path(None);
    proxy_health_impl(&pfile)
}

fn proxy_health_impl(pfile: &PathBuf) -> *mut c_char {
    let pid_and_port = read_pid_and_port(pfile);

    let (port, pid) = match pid_and_port {
        Some((pid, port)) => (port, pid),
        None => {
            std::eprintln!("[nimaproxy] proxy_health: no valid pid in file");
            return std::ptr::null_mut();
        }
    };

    if !is_process_alive(pid) {
        std::eprintln!("[nimaproxy] proxy_health: process {} not alive", pid);
        fs::remove_file(pfile).ok();
        return std::ptr::null_mut();
    }

    std::eprintln!("[nimaproxy] proxy_health: checking port={}", port);

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
    let pfile = pid_file_path(None);
    proxy_stats_impl(&pfile)
}

fn proxy_stats_impl(pfile: &PathBuf) -> *mut c_char {
    let port: u16 = std::fs::read_to_string(pfile)
        .ok()
        .and_then(|s| s.trim().split(':').nth(1).and_then(|p| p.parse().ok()))
        .unwrap_or(8080);

    let pid: libc::pid_t = std::fs::read_to_string(pfile)
        .ok()
        .and_then(|s| s.trim().split(':').next().and_then(|p| p.parse().ok()))
        .unwrap_or(0);

    if pid == 0 || unsafe { libc::kill(pid, 0) } != 0 {
        std::fs::remove_file(pfile).ok();
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::TempDir;

    const NVIDIA_API_KEY: &str = "REDACTED_KEY_1";

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn with_isolated_env<T>(pid: u16, f: impl FnOnce(&str, &str) -> T) -> T {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let unique_id = format!("nimtest-{:016x}-{:08x}", std::process::id(), counter);
        let base_dir = std::path::PathBuf::from(format!("/tmp/{}", unique_id));
        std::fs::create_dir_all(&base_dir).expect("create temp dir");

        let pid_file = base_dir.join("nimaproxy.pid");
        let config_file = base_dir.join("nimaproxy.toml");

        let config = format!(
            r#"listen = "127.0.0.1:{}"
[[keys]]
key = "{}"
label = "test"
"#,
            pid, NVIDIA_API_KEY
        );
        std::fs::write(&config_file, &config).expect("write config");

        let pid_file_str = pid_file.to_str().unwrap();
        std::env::set_var("NIMAPROXY_PID_FILE", pid_file_str);
        set_tls_pid_file(pid_file_str);

        let result = f(config_file.to_str().unwrap(), pid_file_str);

        std::env::remove_var("NIMAPROXY_PID_FILE");
        TLS_PID_FILE.with(|tls| {
            *tls.borrow_mut() = None;
        });

        std::fs::remove_dir_all(&base_dir).ok();
        result
    }

    #[test]
    fn test_proxy_start_stop_cycle() {
        with_isolated_env(19101, |cfg_path, pid_file| {
            let config_path = CString::new(cfg_path).unwrap();
            let pid_file_cstr = CString::new(pid_file).unwrap();
            let result = unsafe { proxy_start_with_pid_file(config_path.as_ptr(), 0, pid_file_cstr.as_ptr()) };
            assert_eq!(result, 0, "proxy_start should succeed");

            std::thread::sleep(std::time::Duration::from_millis(500));
            let pid_content = std::fs::read_to_string(pid_file).unwrap_or_default();
            assert!(!pid_content.is_empty() && pid_content != "starting", "pid file should be written");

            unsafe { proxy_stop() };
        });
    }

    #[test]
    fn test_proxy_health_when_running() {
        with_isolated_env(19102, |cfg_path, pid_file| {
            let config_path = CString::new(cfg_path).unwrap();
            let pid_file_cstr = CString::new(pid_file).unwrap();
            let start_result = unsafe { proxy_start_with_pid_file(config_path.as_ptr(), 0, pid_file_cstr.as_ptr()) };
            assert_eq!(start_result, 0, "proxy_start should succeed");

            std::thread::sleep(std::time::Duration::from_millis(600));

            let health = unsafe { proxy_health() };
            assert!(!health.is_null(), "health should return valid string when running");

            unsafe { proxy_free_string(health) };
            unsafe { proxy_stop() };
        });
    }

    #[test]
    fn test_proxy_stats_when_running() {
        with_isolated_env(19103, |cfg_path, pid_file| {
            let config_path = CString::new(cfg_path).unwrap();
            let pid_file_cstr = CString::new(pid_file).unwrap();
            unsafe { proxy_start_with_pid_file(config_path.as_ptr(), 0, pid_file_cstr.as_ptr()) };
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
        with_isolated_env(19104, |cfg_path, pid_file| {
            let config_path = CString::new(cfg_path).unwrap();
            let pid_file_cstr = CString::new(pid_file).unwrap();
            let result1 = unsafe { proxy_start_with_pid_file(config_path.as_ptr(), 0, pid_file_cstr.as_ptr()) };
            assert_eq!(result1, 0, "first start should succeed");

            std::thread::sleep(std::time::Duration::from_millis(600));

            let result2 = unsafe { proxy_start_with_pid_file(config_path.as_ptr(), 0, pid_file_cstr.as_ptr()) };
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
            let pid_file_cstr = CString::new(pid_file).unwrap();
            let result = unsafe { proxy_start_with_pid_file(config_path.as_ptr(), 19106, pid_file_cstr.as_ptr()) };
            assert_eq!(result, 0, "proxy_start with custom port should succeed");

            std::thread::sleep(std::time::Duration::from_millis(600));

            let pid_content = std::fs::read_to_string(pid_file).unwrap_or_default();
            assert!(pid_content.contains("19106"), "PID file should contain custom port {}", pid_content);

            unsafe { proxy_stop() };
        });
    }
}
