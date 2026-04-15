pub mod config;
pub mod key_pool;
pub mod model_router;
pub mod model_stats;
pub mod proxy;

pub use key_pool::KeyPool;
pub use model_router::{ModelRouter, Strategy};
pub use model_stats::ModelStatsStore;
pub use config::{Config, KeyEntry, RoutingConfig, load as config_load};

use reqwest::Client;

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
    ) -> Self {
        let client = Client::builder()
            .use_rustls_tls()
            .timeout(std::time::Duration::from_secs(120))
            .pool_max_idle_per_host(16)
            .build()
            .expect("failed to build HTTP client");

        AppState {
            pool: KeyPool::new(keys),
            client,
            target,
            router,
            model_stats,
            racing_models,
            racing_max_parallel,
            racing_timeout_ms,
            racing_strategy,
        }
    }
}
