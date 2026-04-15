use nimaproxy::{config, AppState, ModelRouter, ModelStatsStore, Strategy};

use std::sync::Arc;
use axum::{routing, Router};

fn usage() -> ! {
    eprintln!("nimaproxy — NVIDIA NIM key-rotation proxy");
    eprintln!();
    eprintln!("Usage: nimaproxy --config <path> [--port <port>]");
    eprintln!();
    eprintln!("Config file format (TOML):");
    eprintln!("  listen = \"127.0.0.1:8080\"  # optional");
    eprintln!("  target = \"https://...\"      # optional");
    eprintln!("  [[keys]]");
    eprintln!("    key   = \"nvapi-...\"");
    eprintln!("    label = \"bkat\"            # optional");
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut config_path: Option<String> = None;
    let mut port_override: Option<u16> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" | "-c" => {
                i += 1;
                config_path = args.get(i).cloned();
            }
            "--port" | "-p" => {
                i += 1;
                port_override = args.get(i).and_then(|v| v.parse().ok());
            }
            "--help" | "-h" => usage(),
            _ => {}
        }
        i += 1;
    }

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

    let listen = if let Some(p) = port_override {
        format!("127.0.0.1:{}", p)
    } else {
        cfg.listen_addr()
    };

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

    let state = Arc::new(AppState::new(
        keys,
        target.clone(),
        router,
        model_stats,
        racing_models,
        racing_max_parallel,
        racing_timeout_ms,
        racing_strategy,
    ));

    let app = Router::new()
        .route("/v1/chat/completions", routing::post(nimaproxy::proxy::chat_completions))
        .route("/v1/models", routing::get(nimaproxy::proxy::models))
        .route("/health", routing::get(nimaproxy::proxy::health))
        .route("/stats", routing::get(nimaproxy::proxy::stats))
        .with_state(state.clone());

    let key_count = state.pool.len();
    println!("nimaproxy listening on http://{}", listen);
    println!(" target : {}", target);
    println!(" keys   : {} configured", key_count);
    println!(" routes : POST /v1/chat/completions  GET /v1/models  GET /health  GET /stats");

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
