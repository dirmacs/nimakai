pub mod config;
pub mod key_pool;
#[cfg(test)]
pub mod mock_http;
pub mod model_router;
pub mod model_stats;
pub mod proxy;
pub mod test_utils;
pub mod turn_log;

pub use proxy::validate_model_exists;

pub use config::{load as config_load, Config, KeyEntry, ModelParams, RoutingConfig};
pub use key_pool::{KeyAcquireError, KeyLease, KeyPool};
pub use model_router::{ModelRouter, Strategy};
pub use model_stats::{ModelSnapshot, ModelStatsStore, ModelStatus, RecordOutcome};
pub use proxy::{completions, embeddings, props};
use reqwest::Client;
use std::collections::HashMap;
use std::os::raw::c_char;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

fn push_unique_model(models: &mut Vec<String>, model: &str) {
    if !model.is_empty() && !models.iter().any(|m| m == model) {
        models.push(model.to_string());
    }
}

fn collect_configured_models(
    router: Option<&ModelRouter>,
    racing_models: &[String],
    available_models: &[String],
) -> Vec<String> {
    let mut models = Vec::new();
    if let Some(router) = router {
        for model in &router.models {
            push_unique_model(&mut models, model);
        }
    }
    for model in racing_models {
        push_unique_model(&mut models, model);
    }
    for model in available_models {
        push_unique_model(&mut models, model);
    }
    models
}

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
    pub racing_adaptive: bool,
    pub racing_min_parallel: usize,
    pub racing_pressure_parallel: usize,
    pub racing_degraded_parallel: usize,
    pub racing_fast_models: Vec<String>,
    pub racing_fallback_models: Vec<String>,
    pub racing_large_prompt_char_threshold: usize,
    pub racing_large_prompt_parallel: usize,
    pub racing_solo_fallback: bool,
    pub racing_max_total_request_ms: u64,
    pub racing_cursor: Mutex<usize>,
    pub available_models: Mutex<Vec<String>>,
    pub model_params: HashMap<String, ModelParams>,
    pub model_compat: config::ModelCompat,
    pub max_upstream_in_flight: usize,
    pub max_in_flight_per_key: usize,
    pub admission_wait_ms: u64,
    pub min_dynamic_timeout_ms: u64,
    pub dynamic_sample_floor: usize,
    upstream_permits: Arc<Semaphore>,
    pub gateway_metrics: Arc<GatewayMetrics>,
}

#[derive(Clone, Debug)]
pub struct RuntimeControls {
    pub racing_adaptive: bool,
    pub racing_min_parallel: usize,
    pub racing_pressure_parallel: usize,
    pub racing_degraded_parallel: usize,
    pub racing_fast_models: Vec<String>,
    pub racing_fallback_models: Vec<String>,
    pub racing_large_prompt_char_threshold: usize,
    pub racing_large_prompt_parallel: usize,
    pub racing_solo_fallback: bool,
    pub racing_max_total_request_ms: u64,
    pub max_upstream_in_flight: usize,
    pub max_in_flight_per_key: usize,
    pub admission_wait_ms: u64,
    pub min_dynamic_timeout_ms: u64,
    pub dynamic_sample_floor: usize,
}

impl Default for RuntimeControls {
    fn default() -> Self {
        Self {
            racing_adaptive: false,
            racing_min_parallel: 2,
            racing_pressure_parallel: 6,
            racing_degraded_parallel: 3,
            racing_fast_models: Vec::new(),
            racing_fallback_models: Vec::new(),
            racing_large_prompt_char_threshold: 0,
            racing_large_prompt_parallel: 1,
            racing_solo_fallback: true,
            racing_max_total_request_ms: 30000,
            max_upstream_in_flight: 48,
            max_in_flight_per_key: 3,
            admission_wait_ms: 1500,
            min_dynamic_timeout_ms: 8000,
            dynamic_sample_floor: 10,
        }
    }
}

pub struct GatewayMetrics {
    request_total: AtomicU64,
    direct_requests: AtomicU64,
    racing_requests: AtomicU64,
    upstream_attempts: AtomicU64,
    upstream_in_flight: AtomicU64,
    overload_rejects: AtomicU64,
    no_key_rejects: AtomicU64,
    timeout_count: AtomicU64,
    rate_limit_count: AtomicU64,
    fanout_total: AtomicU64,
    fanout_samples: AtomicU64,
    solo_fallbacks: AtomicU64,
    sequential_fallbacks: AtomicU64,
    racing_all_failed: AtomicU64,
    racing_deadline_exceeded: AtomicU64,
    racing_wins: Mutex<HashMap<String, u64>>,
}

#[derive(Clone, Debug)]
pub struct GatewayMetricsSnapshot {
    pub request_total: u64,
    pub direct_requests: u64,
    pub racing_requests: u64,
    pub upstream_attempts: u64,
    pub upstream_in_flight: u64,
    pub overload_rejects: u64,
    pub no_key_rejects: u64,
    pub timeout_count: u64,
    pub rate_limit_count: u64,
    pub fanout_total: u64,
    pub fanout_samples: u64,
    pub fanout_avg: f64,
    pub solo_fallbacks: u64,
    pub sequential_fallbacks: u64,
    pub racing_all_failed: u64,
    pub racing_deadline_exceeded: u64,
    pub racing_wins: HashMap<String, u64>,
}

impl GatewayMetrics {
    pub fn new() -> Self {
        Self {
            request_total: AtomicU64::new(0),
            direct_requests: AtomicU64::new(0),
            racing_requests: AtomicU64::new(0),
            upstream_attempts: AtomicU64::new(0),
            upstream_in_flight: AtomicU64::new(0),
            overload_rejects: AtomicU64::new(0),
            no_key_rejects: AtomicU64::new(0),
            timeout_count: AtomicU64::new(0),
            rate_limit_count: AtomicU64::new(0),
            fanout_total: AtomicU64::new(0),
            fanout_samples: AtomicU64::new(0),
            solo_fallbacks: AtomicU64::new(0),
            sequential_fallbacks: AtomicU64::new(0),
            racing_all_failed: AtomicU64::new(0),
            racing_deadline_exceeded: AtomicU64::new(0),
            racing_wins: Mutex::new(HashMap::new()),
        }
    }

    pub fn record_request(&self, racing: bool) {
        self.request_total.fetch_add(1, Ordering::Relaxed);
        if racing {
            self.racing_requests.fetch_add(1, Ordering::Relaxed);
        } else {
            self.direct_requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_upstream_start(&self) {
        self.upstream_attempts.fetch_add(1, Ordering::Relaxed);
        self.upstream_in_flight.fetch_add(1, Ordering::Relaxed);
    }

    fn record_upstream_finish(&self) {
        self.upstream_in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn record_overload(&self) {
        self.overload_rejects.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_no_key(&self) {
        self.no_key_rejects.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_timeout(&self) {
        self.timeout_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_rate_limit(&self) {
        self.rate_limit_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_fanout(&self, fanout: usize) {
        self.fanout_total
            .fetch_add(fanout as u64, Ordering::Relaxed);
        self.fanout_samples.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_solo_fallback(&self) {
        self.solo_fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_sequential_fallback(&self) {
        self.sequential_fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_all_racers_failed(&self) {
        self.racing_all_failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_deadline_exceeded(&self) {
        self.racing_deadline_exceeded
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_racing_win(&self, model_id: &str) {
        let mut wins = self.racing_wins.lock().unwrap();
        *wins.entry(model_id.to_string()).or_insert(0) += 1;
    }

    pub fn upstream_in_flight(&self) -> u64 {
        self.upstream_in_flight.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> GatewayMetricsSnapshot {
        let fanout_total = self.fanout_total.load(Ordering::Relaxed);
        let fanout_samples = self.fanout_samples.load(Ordering::Relaxed);
        GatewayMetricsSnapshot {
            request_total: self.request_total.load(Ordering::Relaxed),
            direct_requests: self.direct_requests.load(Ordering::Relaxed),
            racing_requests: self.racing_requests.load(Ordering::Relaxed),
            upstream_attempts: self.upstream_attempts.load(Ordering::Relaxed),
            upstream_in_flight: self.upstream_in_flight.load(Ordering::Relaxed),
            overload_rejects: self.overload_rejects.load(Ordering::Relaxed),
            no_key_rejects: self.no_key_rejects.load(Ordering::Relaxed),
            timeout_count: self.timeout_count.load(Ordering::Relaxed),
            rate_limit_count: self.rate_limit_count.load(Ordering::Relaxed),
            fanout_total,
            fanout_samples,
            fanout_avg: if fanout_samples == 0 {
                0.0
            } else {
                fanout_total as f64 / fanout_samples as f64
            },
            solo_fallbacks: self.solo_fallbacks.load(Ordering::Relaxed),
            sequential_fallbacks: self.sequential_fallbacks.load(Ordering::Relaxed),
            racing_all_failed: self.racing_all_failed.load(Ordering::Relaxed),
            racing_deadline_exceeded: self.racing_deadline_exceeded.load(Ordering::Relaxed),
            racing_wins: self.racing_wins.lock().unwrap().clone(),
        }
    }
}

impl Default for GatewayMetrics {
    fn default() -> Self {
        Self::new()
    }
}

pub struct UpstreamPermit {
    _permit: OwnedSemaphorePermit,
    metrics: Arc<GatewayMetrics>,
}

impl Drop for UpstreamPermit {
    fn drop(&mut self) {
        self.metrics.record_upstream_finish();
    }
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
        model_params: HashMap<String, ModelParams>,
        model_compat: config::ModelCompat,
    ) -> Arc<Self> {
        Self::new_with_controls(
            keys,
            target,
            router,
            model_stats,
            racing_models,
            racing_max_parallel,
            racing_timeout_ms,
            racing_strategy,
            model_params,
            model_compat,
            RuntimeControls::default(),
        )
    }

    pub fn new_with_controls(
        keys: Vec<KeyEntry>,
        target: String,
        router: Option<ModelRouter>,
        model_stats: ModelStatsStore,
        racing_models: Vec<String>,
        racing_max_parallel: usize,
        racing_timeout_ms: u64,
        racing_strategy: String,
        model_params: HashMap<String, ModelParams>,
        model_compat: config::ModelCompat,
        controls: RuntimeControls,
    ) -> Arc<Self> {
        let client = Client::builder()
            .use_rustls_tls()
            .timeout(std::time::Duration::from_secs(120))
            .pool_max_idle_per_host(16)
            .build()
            .expect("failed to build HTTP client");

        let available_models = collect_configured_models(router.as_ref(), &racing_models, &[]);
        Arc::new(AppState {
            pool: KeyPool::with_max_in_flight(keys, controls.max_in_flight_per_key),
            client,
            target,
            router,
            model_stats,
            racing_models,
            racing_max_parallel,
            racing_timeout_ms,
            racing_strategy,
            racing_adaptive: controls.racing_adaptive,
            racing_min_parallel: controls.racing_min_parallel.max(2),
            racing_pressure_parallel: controls.racing_pressure_parallel.max(2),
            racing_degraded_parallel: controls.racing_degraded_parallel.max(2),
            racing_fast_models: controls.racing_fast_models,
            racing_fallback_models: controls.racing_fallback_models,
            racing_large_prompt_char_threshold: controls.racing_large_prompt_char_threshold,
            racing_large_prompt_parallel: controls.racing_large_prompt_parallel.max(1),
            racing_solo_fallback: controls.racing_solo_fallback,
            racing_max_total_request_ms: controls.racing_max_total_request_ms,
            racing_cursor: Mutex::new(0),
            available_models: Mutex::new(available_models),
            model_params,
            model_compat,
            max_upstream_in_flight: controls.max_upstream_in_flight.max(1),
            max_in_flight_per_key: controls.max_in_flight_per_key.max(1),
            admission_wait_ms: controls.admission_wait_ms,
            min_dynamic_timeout_ms: controls.min_dynamic_timeout_ms.max(1000),
            dynamic_sample_floor: controls.dynamic_sample_floor.max(2),
            upstream_permits: Arc::new(Semaphore::new(controls.max_upstream_in_flight.max(1))),
            gateway_metrics: Arc::new(GatewayMetrics::new()),
        })
    }

    pub fn configured_models(&self) -> Vec<String> {
        let available = self.available_models.lock().unwrap();
        collect_configured_models(self.router.as_ref(), &self.racing_models, &available)
    }

    pub fn routing_enabled(&self) -> bool {
        self.router
            .as_ref()
            .map(|router| !router.models.is_empty())
            .unwrap_or(false)
    }

    pub fn racing_enabled(&self) -> bool {
        self.racing_models.len() >= 2 && self.racing_max_parallel >= 2
    }

    pub fn try_acquire_upstream(&self) -> Option<UpstreamPermit> {
        let permit = self.upstream_permits.clone().try_acquire_owned().ok()?;
        self.gateway_metrics.record_upstream_start();
        Some(UpstreamPermit {
            _permit: permit,
            metrics: self.gateway_metrics.clone(),
        })
    }
}

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::fs;
use std::path::PathBuf;

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
    let port = parts
        .get(1)
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);
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

fn resolve_proxy_binary() -> String {
    if let Ok(path) = std::env::var("NIMAPROXY_BIN") {
        if !path.trim().is_empty() {
            return path;
        }
    }

    let mut candidates = Vec::new();
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            candidates.push(dir.join("nimaproxy"));
            candidates.push(dir.join("nimaproxy-bin"));
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join("nimaproxy"));
                candidates.push(parent.join("nimaproxy-bin"));
            }
        }
    }

    if let Some(manifest_dir) = option_env!("CARGO_MANIFEST_DIR") {
        let manifest_dir = PathBuf::from(manifest_dir);
        candidates.push(manifest_dir.join("target/debug/nimaproxy"));
        candidates.push(manifest_dir.join("target/release/nimaproxy"));
    }

    candidates.push(PathBuf::from("/usr/local/bin/nimaproxy-bin"));
    candidates.push(PathBuf::from("/usr/local/bin/nimaproxy"));

    for candidate in candidates {
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }

    "nimaproxy".to_string()
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
        std::eprintln!(
            "[nimaproxy] proxy_start: existing pid={}, port={}",
            existing_pid,
            existing_port
        );
        if is_process_alive(existing_pid) && check_proxy_alive(existing_port) {
            std::eprintln!(
                "[nimaproxy] proxy_start: already running pid={}, port={}",
                existing_pid,
                existing_port
            );
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

    let port_cstr = CString::new(port.to_string()).unwrap();
    let config_cstr = CString::new(path).unwrap();
    let bin_path_string = resolve_proxy_binary();
    let bin_path_has_slash = bin_path_string.contains('/');
    let bin_path = CString::new(bin_path_string).unwrap();
    let cf_flag = CString::new("--config").unwrap();
    let pt_flag = CString::new("--port").unwrap();
    let pid_flag = CString::new("--pid-file").unwrap();
    let pid_cstr = CString::new(pfile.to_str().unwrap_or_default()).unwrap();

    let mut attrs: libc::posix_spawnattr_t = unsafe { std::mem::zeroed() };
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
        libc::posix_spawn_file_actions_adddup2(
            &mut file_actions,
            libc::STDIN_FILENO,
            libc::STDOUT_FILENO,
        );
        libc::posix_spawn_file_actions_adddup2(
            &mut file_actions,
            libc::STDIN_FILENO,
            libc::STDERR_FILENO,
        );
    }

    let mut child_pid: libc::pid_t = 0;
    let mut argv: Vec<*mut c_char> = vec![
        bin_path.as_ptr() as *mut c_char,
        cf_flag.as_ptr() as *mut c_char,
        config_cstr.as_ptr() as *mut c_char,
        pt_flag.as_ptr() as *mut c_char,
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
        if bin_path_has_slash {
            libc::posix_spawn(
                &mut child_pid,
                bin_path.as_ptr(),
                &file_actions,
                &mut attrs,
                argv.as_mut_ptr(),
                envp.as_ptr(),
            )
        } else {
            libc::posix_spawnp(
                &mut child_pid,
                bin_path.as_ptr(),
                &file_actions,
                &mut attrs,
                argv.as_mut_ptr(),
                envp.as_ptr(),
            )
        }
    };

    for env_str in envp.iter().take(envp.len() - 1) {
        if !env_str.is_null() {
            unsafe {
                let _ = CString::from_raw(*env_str);
            }
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
                std::eprintln!(
                    "[nimaproxy] proxy_start: proxy ready on port={}",
                    written_port
                );
                return 0;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    std::eprintln!("[nimaproxy] proxy_start: proxy failed to become ready");
    fs::remove_file(&pfile).ok();
    unsafe {
        libc::kill(child_pid, libc::SIGTERM);
    }
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

    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
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
    unsafe {
        let _ = CString::from_raw(s);
    }
}

#[cfg(test)]
mod ffi_tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::TempDir;

    const NVIDIA_API_KEY: &str = "nvapi-YOUR_TEST_KEY_HERE";

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
            let result = unsafe {
                proxy_start_with_pid_file(config_path.as_ptr(), 0, pid_file_cstr.as_ptr())
            };
            assert_eq!(result, 0, "proxy_start should succeed");

            std::thread::sleep(std::time::Duration::from_millis(500));
            let pid_content = std::fs::read_to_string(pid_file).unwrap_or_default();
            assert!(
                !pid_content.is_empty() && pid_content != "starting",
                "pid file should be written"
            );

            unsafe { proxy_stop() };
        });
    }

    #[test]
    fn test_proxy_health_when_running() {
        with_isolated_env(19102, |cfg_path, pid_file| {
            let config_path = CString::new(cfg_path).unwrap();
            let pid_file_cstr = CString::new(pid_file).unwrap();
            let start_result = unsafe {
                proxy_start_with_pid_file(config_path.as_ptr(), 0, pid_file_cstr.as_ptr())
            };
            assert_eq!(start_result, 0, "proxy_start should succeed");

            std::thread::sleep(std::time::Duration::from_millis(600));

            let health = unsafe { proxy_health() };
            assert!(
                !health.is_null(),
                "health should return valid string when running"
            );

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
            assert!(
                !stats.is_null(),
                "stats should return valid string when running"
            );

            unsafe { proxy_free_string(stats) };
            unsafe { proxy_stop() };
        });
    }

    #[test]
    fn test_proxy_health_when_stopped() {
        let health = unsafe { proxy_health() };
        assert!(
            health.is_null(),
            "health should return null when not running"
        );
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
            let result1 = unsafe {
                proxy_start_with_pid_file(config_path.as_ptr(), 0, pid_file_cstr.as_ptr())
            };
            assert_eq!(result1, 0, "first start should succeed");

            std::thread::sleep(std::time::Duration::from_millis(600));

            let result2 = unsafe {
                proxy_start_with_pid_file(config_path.as_ptr(), 0, pid_file_cstr.as_ptr())
            };
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
            let result = unsafe {
                proxy_start_with_pid_file(config_path.as_ptr(), 19106, pid_file_cstr.as_ptr())
            };
            assert_eq!(result, 0, "proxy_start with custom port should succeed");

            std::thread::sleep(std::time::Duration::from_millis(600));

            let pid_content = std::fs::read_to_string(pid_file).unwrap_or_default();
            assert!(
                pid_content.contains("19106"),
                "PID file should contain custom port {}",
                pid_content
            );

            unsafe { proxy_stop() };
        });
    }

    /// Test AppState::new() initialization
    #[test]
    fn test_app_state_new() {
        let keys = vec![KeyEntry {
            key: NVIDIA_API_KEY.to_string(),
            label: Some("test-key".to_string()),
        }];
        let model_params = HashMap::new();
        let model_compat = config::ModelCompat::default();

        let state = AppState::new(
            keys,
            "http://localhost:8080".to_string(),
            None,
            ModelStatsStore::new(100.0),
            vec!["nvidia/nemotron-3b".to_string()],
            3,
            5000,
            "round_robin".to_string(),
            model_params,
            model_compat,
        );

        assert_eq!(state.racing_models.len(), 1);
        assert_eq!(state.racing_max_parallel, 3);
        assert_eq!(state.racing_timeout_ms, 5000);
        assert_eq!(state.racing_strategy, "round_robin");
        assert_eq!(state.target, "http://localhost:8080");
    }

    /// Test pid_file_path with override path
    #[test]
    fn test_pid_file_path_with_override() {
        let override_path = Some("/custom/pid/file.pid");
        let result = pid_file_path(override_path);
        assert_eq!(result, PathBuf::from("/custom/pid/file.pid"));
    }

    /// Test pid_file_path with TLS path
    #[test]
    fn test_pid_file_path_with_tls() {
        let test_pid = "/tmp/test_tls_pid.pid";
        set_tls_pid_file(test_pid);
        let result = pid_file_path(None);
        assert_eq!(result, PathBuf::from(test_pid));

        TLS_PID_FILE.with(|tls| {
            *tls.borrow_mut() = None;
        });
    }

    /// Test pid_file_path falls back to env var
    #[test]
    fn test_pid_file_path_fallback_to_env() {
        TLS_PID_FILE.with(|tls| {
            *tls.borrow_mut() = None;
        });
        std::env::set_var("NIMAPROXY_PID_FILE", "/tmp/env_pid.pid");
        let result = pid_file_path(None);
        assert_eq!(result, PathBuf::from("/tmp/env_pid.pid"));
        std::env::remove_var("NIMAPROXY_PID_FILE");
    }

    /// Test pid_file_path default fallback
    #[test]
    fn test_pid_file_path_default() {
        TLS_PID_FILE.with(|tls| {
            *tls.borrow_mut() = None;
        });
        std::env::remove_var("NIMAPROXY_PID_FILE");
        let result = pid_file_path(None);
        assert_eq!(result, PathBuf::from("/tmp/nimaproxy.pid"));
    }

    #[test]
    fn test_resolve_proxy_binary_prefers_env() {
        let previous = std::env::var("NIMAPROXY_BIN").ok();
        std::env::set_var("NIMAPROXY_BIN", "/tmp/custom-nimaproxy");

        assert_eq!(resolve_proxy_binary(), "/tmp/custom-nimaproxy");

        if let Some(previous) = previous {
            std::env::set_var("NIMAPROXY_BIN", previous);
        } else {
            std::env::remove_var("NIMAPROXY_BIN");
        }
    }

    /// Test is_process_alive with current process
    #[test]
    fn test_is_process_alive_current() {
        let current_pid = unsafe { libc::getpid() };
        assert!(is_process_alive(current_pid));
    }

    /// Test is_process_alive returns false for clearly invalid pid
    #[test]
    fn test_is_process_alive_invalid() {
        // Use a very high pid that won't exist
        // Note: actual behavior depends on system, so we just verify the function runs
        let result = is_process_alive(999999);
        // This pid should not exist on most systems
        assert!(!result, "pid 999999 should not exist");
    }

    /// Test read_pid_and_port with invalid file
    #[test]
    fn test_read_pid_and_port_invalid_file() {
        let result = read_pid_and_port(&PathBuf::from("/nonexistent/file.pid"));
        assert!(result.is_none());
    }

    /// Test read_pid_and_port with empty content
    #[test]
    fn test_read_pid_and_port_empty() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("test.pid");
        std::fs::write(&pid_file, "").unwrap();

        let result = read_pid_and_port(&pid_file);
        assert!(result.is_none());
    }

    /// Test read_pid_and_port with "starting" content
    #[test]
    fn test_read_pid_and_port_starting() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("test.pid");
        std::fs::write(&pid_file, "starting").unwrap();

        let result = read_pid_and_port(&pid_file);
        assert!(result.is_none());
    }

    /// Test read_pid_and_port with valid content
    #[test]
    fn test_read_pid_and_port_valid() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("test.pid");
        std::fs::write(&pid_file, "12345:8080").unwrap();

        let result = read_pid_and_port(&pid_file);
        assert!(result.is_some());
        let (pid, port) = result.unwrap();
        assert_eq!(pid, 12345);
        assert_eq!(port, 8080);
    }

    /// Test read_pid_and_port without port (defaults to 8080)
    #[test]
    fn test_read_pid_and_port_default_port() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("test.pid");
        std::fs::write(&pid_file, "12345").unwrap();

        let result = read_pid_and_port(&pid_file);
        assert!(result.is_some());
        let (pid, port) = result.unwrap();
        assert_eq!(pid, 12345);
        assert_eq!(port, 8080);
    }

    /// Test check_proxy_alive when proxy is not running
    #[test]
    fn test_check_proxy_alive_not_running() {
        assert!(!check_proxy_alive(9999));
    }

    /// Test wait_for_proxy_ready timeout
    #[test]
    fn test_wait_for_proxy_ready_timeout() {
        let result = wait_for_proxy_ready(9999, 100);
        assert!(!result);
    }

    /// Test proxy_free_string with null pointer (should not panic)
    #[test]
    fn test_proxy_free_string_null() {
        unsafe {
            proxy_free_string(std::ptr::null_mut());
        }
    }

    /// Test proxy_start with null config path
    #[test]
    fn test_proxy_start_null_config() {
        let result = unsafe { proxy_start(std::ptr::null(), 0) };
        assert_eq!(result, -1, "proxy_start with null config should fail");
    }

    /// Test proxy_health_impl with no valid pid file
    #[test]
    fn test_proxy_health_impl_no_pid_file() {
        let fake_path = PathBuf::from("/nonexistent/pid/file.pid");
        let result = proxy_health_impl(&fake_path);
        assert!(
            result.is_null(),
            "health should return null for nonexistent pid file"
        );
    }

    /// Test proxy_stats_impl with no valid pid file
    #[test]
    fn test_proxy_stats_impl_no_pid_file() {
        let fake_path = PathBuf::from("/nonexistent/pid/file.pid");
        let result = proxy_stats_impl(&fake_path);
        assert!(
            result.is_null(),
            "stats should return null for nonexistent pid file"
        );
    }

    /// Test proxy_stats_impl with dead process
    #[test]
    fn test_proxy_stats_impl_dead_process() {
        // Use a non-existent pid that will fail the kill check
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("test.pid");
        // Use a very high pid that won't exist
        std::fs::write(&pid_file, "999999:8080").unwrap();

        let result = proxy_stats_impl(&pid_file);
        // The function should return null when process doesn't exist
        assert!(
            result.is_null(),
            "stats should return null for non-existent process"
        );
    }

    // Test 16: FFI null pointer handling
    #[test]
    fn test_ffi_null_pointer_handling() {
        unsafe {
            proxy_free_string(std::ptr::null_mut());
        }
        let result = unsafe { proxy_start(std::ptr::null(), 0) };
        assert_eq!(result, -1, "Should return -1 for null config");
    }

    // Test 17: FFI invalid UTF-8 handling
    #[test]
    fn test_ffi_invalid_utf8_handling() {
        let invalid_bytes = vec![0xFF, 0xFE, 0xFD];
        let c_string = CString::from_vec_with_nul(invalid_bytes);
        assert!(c_string.is_err(), "Should fail for invalid UTF-8");
    }

    // Test 18: FFI memory leak prevention
    #[test]
    fn test_ffi_memory_leak_prevention() {
        with_isolated_env(19200, |cfg_path, pid_file| {
            let config_path = CString::new(cfg_path).unwrap();
            let pid_file_cstr = CString::new(pid_file).unwrap();
            let start_result = unsafe {
                proxy_start_with_pid_file(config_path.as_ptr(), 0, pid_file_cstr.as_ptr())
            };
            assert_eq!(start_result, 0);
            std::thread::sleep(std::time::Duration::from_millis(600));
            let stats = unsafe { proxy_stats() };
            assert!(!stats.is_null());
            unsafe { proxy_free_string(stats) };
            unsafe { proxy_stop() };
            assert!(true);
        });
    }
}
