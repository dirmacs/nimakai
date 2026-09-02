use axum::{http::StatusCode, response::IntoResponse, routing::get, routing::post, Router};
use nimaproxy::turn_log;
use nimaproxy::{
    config, model_refresh, AppState, ModelRouter, ModelStatsStore, RuntimeControls, Strategy,
};
use tracing::{info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

fn usage() -> ! {
    eprintln!("nimaproxy — NVIDIA NIM key-rotation proxy");
    eprintln!();
    eprintln!("Usage: nimaproxy --config <path> [--port <port>] [--pid-file <path>]");
    eprintln!();
    eprintln!("Config file format (TOML):");
    eprintln!("  listen = \"127.0.0.1:8080\" # optional");
    eprintln!("  target = \"https://...\" # optional");
    eprintln!("  [[keys]]");
    eprintln!("  key = \"nvapi-...\"");
    eprintln!("  label = \"bkat\" # optional");
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    // Parse args first to get config path and port override
    let args: Vec<String> = std::env::args().collect();
    let mut config_path: Option<String> = None;
    let mut port_override: Option<u16> = None;
    let mut pid_file_override: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" | "-c" => {
                i += 1;
                config_path = args.get(i).cloned();
            }
            "--port" | "-p" => {
                i += 1;
                if let Some(p) = args.get(i).and_then(|v| v.parse::<u16>().ok()) {
                    port_override = Some(p);
                }
            }
            "--pid-file" => {
                i += 1;
                pid_file_override = args.get(i).cloned();
            }
            "--help" | "-h" => usage(),
            _ => {}
        }
        i += 1;
    }

    if let Some(ref pf) = pid_file_override {
        std::env::set_var("NIMAPROXY_PID_FILE", pf);
    }

    let pid_file_path =
        std::env::var("NIMAPROXY_PID_FILE").unwrap_or_else(|_| "/tmp/nimaproxy.pid".to_string());

    // Initialize tracing early for debugging
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,nimaproxy=debug"));
    let _ = tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_target(true)
                .with_thread_ids(true)
                .with_file(true)
                .with_line_number(true),
        )
        .with(filter)
        .try_init();

    info!("nimaproxy starting up");

    // Load config to determine actual port
    let config_path = config_path.unwrap_or_else(|| "nimaproxy.toml".to_string());
    let cfg = match config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    if cfg.keys.is_empty() {
        eprintln!("error: no keys defined in config — add at least one [[keys]] entry");
        std::process::exit(1);
    }

    if cfg.logging_enabled() {
        let log_path = cfg.logging_path();
        match turn_log::init_logger(&log_path, true) {
            Ok(()) => info!(path = %log_path, "Turn logging initialized"),
            Err(e) => warn!(path = %log_path, error = %e, "Turn logging disabled"),
        }
    }

    // Determine actual listen address and port
    // Treat port_override=0 as "use config default" (same as None)
    let listen = if let Some(p) = port_override.filter(|&p| p != 0) {
        format!("127.0.0.1:{}", p)
    } else {
        cfg.listen_addr()
    };
    let port: u16 = listen
        .split(':')
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    // CRITICAL: Write PID file AFTER determining actual port, BEFORE binding TCP.
    // Parent polls for: (1) PID file with correct PID:PORT, (2) TCP port accepting connections.
    let pid = std::process::id();
    let pid_content = format!("{}:{}", pid, port);
    if let Err(e) = std::fs::write(&pid_file_path, &pid_content) {
        eprintln!("[nimaproxy main] FAILED to write PID file: {}", e);
    } else {
        eprintln!(
            "[nimaproxy main] WROTE PID FILE: {} -> {}",
            pid_file_path, pid_content
        );
    }

    let target = cfg.target_url();

    // --- Startup model refresh (fail-open) ---
    // Fetch the upstream /v1/models catalog with a pool key BEFORE AppState is constructed,
    // and prune every configured model list (routing, racing, fast, fallback) of ids that are
    // not present upstream. A fetch failure leaves the configured model lists unchanged and
    // never blocks startup.
    let pool_key = cfg.keys[0].key.clone();
    let upstream_ids = match model_refresh::fetch_upstream_model_ids(&target, &pool_key).await {
        Ok(ids) => {
            info!(
                count = ids.len(),
                "startup model refresh: fetched upstream /v1/models catalog"
            );
            Some(ids)
        }
        Err(e) => {
            warn!(
                error = %e,
                "startup model refresh failed; keeping configured model lists unchanged (fail-open)"
            );
            None
        }
    };
    let mut pruned_models: Vec<String> = Vec::new();

    let (router, model_stats) = match &cfg.routing {
        Some(r) if !r.models.as_ref().map_or(true, |m| m.is_empty()) => {
            let threshold = r.spike_threshold_ms.unwrap_or(3000.0);
            let strategy = r
                .strategy
                .as_deref()
                .map(Strategy::from_str)
                .unwrap_or(Strategy::RoundRobin);
            let mut models = r.models.clone().unwrap_or_default();
            if let Some(ids) = &upstream_ids {
                models =
                    model_refresh::prune_and_log(models, ids, "routing.models", &mut pruned_models);
            }
            let stats = ModelStatsStore::new(threshold);
            let emptied_by_prune = models.is_empty();
            let router = ModelRouter::new(models, strategy);
            if emptied_by_prune {
                warn!("routing pool emptied by upstream model prune; falling back to passthrough routing (same as an empty config)");
            }
            (Some(router), stats)
        }
        _ => (None, ModelStatsStore::new(3000.0)),
    };

    let mut racing_models = cfg.racing_models();
    if let Some(ids) = &upstream_ids {
        let had_enough_racers = racing_models.len() >= 2;
        racing_models =
            model_refresh::prune_and_log(racing_models, ids, "racing.models", &mut pruned_models);
        if had_enough_racers && racing_models.len() < 2 {
            warn!("racing pool degraded below 2 usable models by upstream prune; falling back to solo/passthrough racing (same as an empty config)");
        }
    }
    let racing_max_parallel = cfg.racing_max_parallel();
    let racing_timeout_ms = cfg.racing_timeout_ms();
    let racing_strategy = cfg.racing_strategy();
    let mut runtime_controls = RuntimeControls {
        racing_adaptive: cfg.racing_adaptive(),
        racing_min_parallel: cfg.racing_min_parallel(),
        racing_pressure_parallel: cfg.racing_pressure_parallel(),
        racing_degraded_parallel: cfg.racing_degraded_parallel(),
        racing_fast_models: cfg.racing_fast_models(),
        racing_fallback_models: cfg.racing_fallback_models(),
        racing_large_prompt_char_threshold: cfg.racing_large_prompt_char_threshold(),
        racing_large_prompt_parallel: cfg.racing_large_prompt_parallel(),
        racing_solo_fallback: cfg.racing_solo_fallback(),
        racing_max_total_request_ms: cfg.racing_max_total_request_ms(),
        max_upstream_in_flight: cfg.max_upstream_in_flight(),
        max_in_flight_per_key: cfg.max_in_flight_per_key(),
        admission_wait_ms: cfg.admission_wait_ms(),
        min_dynamic_timeout_ms: cfg.min_dynamic_timeout_ms(),
        dynamic_sample_floor: cfg.dynamic_sample_floor(),
    };
    if let Some(ids) = &upstream_ids {
        runtime_controls.racing_fast_models = model_refresh::prune_and_log(
            runtime_controls.racing_fast_models,
            ids,
            "racing.fast_models",
            &mut pruned_models,
        );
        runtime_controls.racing_fallback_models = model_refresh::prune_and_log(
            runtime_controls.racing_fallback_models,
            ids,
            "racing.fallback_models",
            &mut pruned_models,
        );
    }
    let model_check_interval_secs = cfg.racing_model_check_interval_secs();

    pruned_models.sort();
    pruned_models.dedup();
    if !pruned_models.is_empty() {
        info!(
            pruned_count = pruned_models.len(),
            pruned = ?pruned_models,
            "startup model refresh: pruned models not present upstream"
        );
    } else if upstream_ids.is_some() {
        info!("startup model refresh: all configured models present upstream");
    }

    let keys = cfg.keys;
    let model_params = cfg.model_params.unwrap_or_default();
    let model_compat = cfg.model_compat.unwrap_or_default();

    eprintln!("[nimaproxy main] model_compat loaded: supports_developer_role={:?}, supports_tool_messages={:?}", 
        model_compat.supports_developer_role, model_compat.supports_tool_messages);

    let state = AppState::new_with_controls(
        keys,
        target.clone(),
        router,
        model_stats,
        racing_models,
        racing_max_parallel,
        racing_timeout_ms,
        racing_strategy,
        model_params,
        model_compat,
        runtime_controls,
    );

    if !pruned_models.is_empty() {
        if let Ok(mut guard) = state.pruned_models.lock() {
            *guard = pruned_models.clone();
        }
    }

    if model_check_interval_secs > 0 {
        let recheck_state = state.clone();
        let recheck_target = target.clone();
        let recheck_key = pool_key.clone();
        tokio::spawn(async move {
            model_refresh::run_periodic_recheck(
                recheck_state,
                recheck_target,
                recheck_key,
                model_check_interval_secs,
            )
            .await;
        });
        info!(
            interval_secs = model_check_interval_secs,
            "periodic model recheck task scheduled"
        );
    } else {
        info!("periodic model recheck task disabled (model_check_interval_secs=0)");
    }

    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(nimaproxy::proxy::chat_completions),
        )
        .route("/test-post", post(nimaproxy::proxy::chat_completions))
        .route("/v1/models", get(nimaproxy::proxy::models))
        .route("/models", get(nimaproxy::proxy::models)) // alias: OMP polls /models without /v1/ prefix
        .route("/health", get(nimaproxy::proxy::health))
        .route("/stats", get(nimaproxy::proxy::stats))
        .route("/v1/completions", post(nimaproxy::proxy::completions))
        .route("/v1/embeddings", post(nimaproxy::proxy::embeddings))
        .route("/props", get(nimaproxy::proxy::props))
        .fallback(fallback_handler)
        .with_state(state.clone());

    async fn fallback_handler(
        uri: axum::http::Uri,
        method: axum::http::Method,
    ) -> impl IntoResponse {
        warn!(uri = %uri, method = %method, "unmatched route - 404");
        (
            StatusCode::NOT_FOUND,
            format!("No route for {} {}", method, uri),
        )
    }

    let key_count = state.pool.len();
    println!("nimaproxy listening on http://{}", listen);
    println!("  target : {}", target);
    println!("  keys   : {} configured", key_count);

    if let Some(ref r) = cfg.routing {
        if let Some(ref models) = r.models {
            if !models.is_empty() {
                let strategy = r.strategy.as_deref().unwrap_or("round_robin");
                let threshold = r.spike_threshold_ms.unwrap_or(3000.0);
                println!(
                    "  routing: {} strategy, {} models, spike>{:.0}ms",
                    strategy,
                    models.len(),
                    threshold
                );
            }
        }
    }

    if !state.racing_models.is_empty() {
        println!(
            "  racing : {} models, max_parallel={}, timeout={}ms, total_deadline={}ms, strategy={}, adaptive={}",
            state.racing_models.len(),
            state.racing_max_parallel,
            state.racing_timeout_ms,
            state.racing_max_total_request_ms,
            state.racing_strategy,
            state.racing_adaptive
        );
    }

    println!(
        "  limits : upstream={}, per_key={}, admission_wait={}ms, timeout_floor={}ms, sample_floor={}",
        state.max_upstream_in_flight,
        state.max_in_flight_per_key,
        state.admission_wait_ms,
        state.min_dynamic_timeout_ms,
        state.dynamic_sample_floor
    );
    if state.racing_large_prompt_char_threshold > 0 {
        println!(
            "  uptime : large_prompt_threshold={}, large_prompt_parallel={}, solo_fallback={}",
            state.racing_large_prompt_char_threshold,
            state.racing_large_prompt_parallel,
            state.racing_solo_fallback
        );
    }

    println!("  routes : POST /v1/chat/completions POST /v1/completions POST /v1/embeddings GET /v1/models GET /props GET /health GET /stats");

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| {
            eprintln!("cannot bind to {}: {}", listen, e);
            std::process::exit(1);
        });

    axum::serve(listener, app)
        .await
        .unwrap_or_else(|e| eprintln!("server error: {}", e));
}
