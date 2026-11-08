mod handlers;
mod models;
mod rate_limiter;
mod ytdl;

use handlers::{get_video_info, resolve_format, stream_proxy, AppState};
use rate_limiter::RateLimiter;
use ytdl::YtdlManager;

use axum::{
    routing::{get, post},
    Router,
};
use reqwest::Client;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ytdl_raw=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // 1 vCPU VPS Concurrency Cap: Max 3 concurrent child yt-dlp instances
    let max_concurrency = std::env::var("MAX_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(3);

    // Per-IP Rate Limiting: 10 requests / 60 seconds
    let rate_limit_rpm = std::env::var("RATE_LIMIT_RPM")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(10);

    info!("Initializing YTDL-RAW Systems Engine");
    info!("Concurrency cap: {} simultaneous processes", max_concurrency);
    info!("Rate limiter: {} req/min per client IP", rate_limit_rpm);

    let ytdl_manager = Arc::new(YtdlManager::new(max_concurrency));
    let rate_limiter = Arc::new(RateLimiter::new(rate_limit_rpm, Duration::from_secs(60)));

    // Reusable streaming client with HTTP/2 and connection pooling
    let http_client = Client::builder()
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .connect_timeout(Duration::from_secs(10))
        .local_address(Some(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)))
        .build()?;

    let app_state = AppState {
        ytdl: ytdl_manager,
        rate_limiter,
        http_client,
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Build Axum routes
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/api/info", post(get_video_info))
        .route("/api/resolve", post(resolve_format))
        .route("/api/proxy", get(stream_proxy))
        .fallback_service(ServeDir::new("public"))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(app_state);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("=====================================================");
    info!(" RAW YOUTUBE STREAM EXTRACTOR — LISTENING ON {}", addr);
    info!(" ZERO MEDIA STORAGE PHILOSOPHY ACTIVE");
    info!("=====================================================");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("Server shutdown completed gracefully.");
    Ok(())
}

async fn health_check() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "online",
        "system": "ytdl-raw",
        "philosophy": "zero-media-disk-storage",
        "timestamp": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Termination signal received. Draining connections...");
}
