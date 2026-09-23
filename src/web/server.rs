use crate::config::settings::Settings;
use crate::web::auth::{
    handle_change_password, handle_create_user, handle_delete_user, handle_list_users,
    handle_login, handle_logout, handle_me, handle_reset_password,
};
use crate::web::equity_log::{handle_equity_curve, SAMPLE_INTERVAL_SECS};
use crate::web::handlers::*;
use crate::web::service::WebTradingService;
use anyhow::Result;
use axum::routing::{delete, get, post};
use axum::Router;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;
use tracing::info;

pub fn create_router(service: Arc<WebTradingService>) -> Router {
    Router::new()
        .route("/", get(handle_index))
        .route("/static/*path", get(handle_static))
        .route("/api/status", get(handle_status))
        .route("/api/instruments", get(handle_instruments))
        .route("/api/candles", get(handle_candles))
        .route("/api/account", get(handle_account))
        .route("/api/account/equity", get(handle_equity_curve))
        .route("/api/history/decisions", get(handle_get_decision_history))
        .route("/api/history/decisions/:record_id", delete(handle_delete_decision_history))
        .route("/api/history/trades", get(handle_get_trade_history))
        .route("/api/history/trades/:record_id", delete(handle_delete_trade_history))
        .route("/api/analyze", post(handle_analyze))
        .route("/api/automation", post(handle_automation))
        .route("/api/config", get(handle_get_config))
        .route("/api/config/save_env", post(handle_save_config))
        .route("/api/trading_system", post(handle_set_trading_system))
        .route("/api/contract/specs", get(handle_contract_specs))
        .route("/api/trade/cancel", post(handle_cancel_order))
        .route("/api/trade/cancel_all", post(handle_cancel_all_orders))
        .route("/api/auth/login", post(handle_login))
        .route("/api/auth/logout", post(handle_logout))
        .route("/api/auth/me", get(handle_me))
        .route("/api/auth/password", post(handle_change_password))
        .route("/api/auth/users", get(handle_list_users).post(handle_create_user))
        .route("/api/auth/users/:username", delete(handle_delete_user))
        .route("/api/auth/users/password", post(handle_reset_password))
        .route("/bridge/sync", axum::routing::post(handle_bridge_sync))
        .layer(axum::middleware::from_fn_with_state(
            service.clone(),
            crate::web::auth::auth_middleware,
        ))
        .layer(CorsLayer::permissive())
        .with_state(service)
}

pub async fn run_server(host: &str, port: u16, settings: Settings) -> Result<()> {
    let service = Arc::new(WebTradingService::new(settings.clone()));

    // Spawn background automation tick loop
    let poll_seconds = settings.mt5.automation_poll_seconds.max(5);
    let auto_service = Arc::clone(&service);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(poll_seconds));
        loop {
            interval.tick().await;
            if let Err(e) = auto_service.automation_tick().await {
                tracing::warn!("Automation loop error: {}", e);
            }
        }
    });

    // Spawn background equity sampling loop (方案 A：每 60s 采样一次权益快照)
    let equity_service = Arc::clone(&service);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(SAMPLE_INTERVAL_SECS));
        loop {
            interval.tick().await;
            if let Some(point) = equity_service.sample_equity_now() {
                equity_service.equity_log.record(point);
            }
        }
    });

    let app = create_router(service);
    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    info!("Starting mt5-2pa-agent server on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
