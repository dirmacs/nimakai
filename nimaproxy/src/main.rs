use nimaproxy::{config, AppState, ModelRouter, ModelStatsStore, Strategy};
use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    routing::post,
    Router,
};
use tracing::{info, warn};
use nimaproxy::turn_log;
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

    let pid_file_path = std::env::var("NIMAPROXY_PID_FILE")
        .unwrap_or_else(|_| "/tmp/nimaproxy.pid".to_string());

    // Initialize tracing early for debugging
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,nimaproxy=debug"));
    let _ = tracing_subscriber::registry()
        .with(fmt::layer().with_target(true).with_thread_ids(true).with_file(true).with_line_number(true))
        .with(filter)
        .try_init();

    info!("nimaproxy starting up");

// Initialize turn logging
let _ = turn_log::init_logger("/var/log/nimaproxy/turns.jsonl", true);
info!("Turn logging initialized");

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

    // Determine actual listen address and port
    // Treat port_override=0 as "use config default" (same as None)
    let listen = if let Some(p) = port_override.filter(|&p| p != 0) {
        format!("127.0.0.1:{}", p)
    } else {
        cfg.listen_addr()
    };
    let port: u16 = listen.split(':').nth(1).and_then(|p| p.parse().ok()).unwrap_or(8080);

    // CRITICAL: Write PID file AFTER determining actual port, BEFORE binding TCP.
    // Parent polls for: (1) PID file with correct PID:PORT, (2) TCP port accepting connections.
    let pid = std::process::id();
    let pid_content = format!("{}:{}", pid, port);
    if let Err(e) = std::fs::write(&pid_file_path, &pid_content) {
        eprintln!("[nimaproxy main] FAILED to write PID file: {}", e);
    } else {
        eprintln!("[nimaproxy main] WROTE PID FILE: {} -> {}", pid_file_path, pid_content);
    }

    let target = cfg.target_url();

    let (router, model_stats) = match &cfg.routing {
        Some(r) if !r.models.as_ref().map_or(true, |m| m.is_empty()) => {
            let threshold = r.spike_threshold_ms.unwrap_or(3000.0);
            let strategy = r
                .strategy
                .as_deref()
                .map(Strategy::from_str)
                .unwrap_or(Strategy::RoundRobin);
            let models = r.models.clone().unwrap_or_default();
            let stats = ModelStatsStore::new(threshold);
            let router = ModelRouter::new(models, strategy);
            (Some(router), stats)
        }
        _ => (None, ModelStatsStore::new(3000.0)),
    };

    let racing_models = cfg.racing_models();
    let racing_max_parallel = cfg.racing_max_parallel();
    let racing_timeout_ms = cfg.racing_timeout_ms();
    let racing_strategy = cfg.racing_strategy();
    let keys = cfg.keys;
    let model_params = cfg.model_params.unwrap_or_default();
    let model_compat = cfg.model_compat.unwrap_or_default();

    eprintln!("[nimaproxy main] model_compat loaded: supports_developer_role={:?}, supports_tool_messages={:?}", 
        model_compat.supports_developer_role, model_compat.supports_tool_messages);

    let state = AppState::new(
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
    );

    let app = Router::new()
        .route("/v1/chat/completions", post(nimaproxy::proxy::chat_completions))
        .route("/test-post", post(nimaproxy::proxy::chat_completions))
        .route("/v1/models", get(nimaproxy::proxy::models))
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
        (StatusCode::NOT_FOUND, format!("No route for {} {}", method, uri))
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
                println!("  routing: {} strategy, {} models, spike>{:.0}ms", strategy, models.len(), threshold);
            }
        }
    }

    if !state.racing_models.is_empty() {
        println!("  racing : {} models, max_parallel={}, timeout={}ms, strategy={}",
            state.racing_models.len(), state.racing_max_parallel, state.racing_timeout_ms, state.racing_strategy);
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
