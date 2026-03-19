#![cfg(not(feature = "s3"))]

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use axum::Router;
use metrics_exporter_prometheus::PrometheusHandle;

use streaminfa::control::events;
use streaminfa::control::state::StreamStateManager;
use streaminfa::core::auth::AuthProvider;
use streaminfa::core::config::AppConfig;
use streaminfa::delivery::router::{self, AppState};
use streaminfa::storage::cache::ObjectCache;
use streaminfa::storage::memory::InMemoryMediaStore;

pub const TEST_ADMIN_TOKEN: &str = "at_phase_e_test_token";

fn test_metrics_handle() -> PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE
        .get_or_init(streaminfa::observability::metrics::install_prometheus_recorder)
        .clone()
}

pub fn build_test_app(require_auth: bool) -> (Router, AppState) {
    let mut config = AppConfig::default();
    if require_auth {
        config.auth.admin_bearer_tokens = vec![TEST_ADMIN_TOKEN.to_string()];
    } else {
        config.auth.admin_bearer_tokens.clear();
    }

    let store = Arc::new(InMemoryMediaStore::new());
    let cache = Arc::new(ObjectCache::new(&config.cache));
    let state_manager = Arc::new(StreamStateManager::new());
    let auth = Arc::new(AuthProvider::new(&config.auth));
    let (event_tx, _event_rx) = events::create_event_channel();

    let state = AppState {
        store,
        cache,
        state_manager,
        auth,
        config: config.clone(),
        start_time: Instant::now(),
        metrics_handle: test_metrics_handle(),
        event_tx,
    };

    let app = router::build_router(state.clone(), &config.delivery, &config.security);
    (app, state)
}
