pub mod config;
pub mod key_pool;
pub mod model_router;
pub mod model_stats;
pub mod proxy;

pub use key_pool::KeyPool;
pub use model_router::{ModelRouter, Strategy};
pub use model_stats::ModelStatsStore;
pub use config::{Config, KeyEntry, RoutingConfig, load as config_load};

use std::sync::Arc;
use reqwest::Client;

pub struct AppState {
    pub pool: KeyPool,
    pub client: Client,
    pub target: String,
    pub router: Option<ModelRouter>,
    pub model_stats: ModelStatsStore,
}

impl AppState {
    pub fn new(
        keys: Vec<KeyEntry>,
        target: String,
        router: Option<ModelRouter>,
        model_stats: ModelStatsStore,
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
        }
    }
}
