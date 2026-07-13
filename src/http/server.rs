use crate::channel::ChannelManager;
use crate::config::Config;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

#[derive(Clone)]
struct AppState {
    cache_dir: PathBuf,
    channels: ChannelManager,
}

#[derive(Serialize)]
struct ChannelsResponse {
    channels: Vec<ChannelInfo>,
}

#[derive(Serialize)]
struct ChannelInfo {
    id: String,
    mpd: String,
}

/// Serve live DASH manifests, media segments, and channel status over HTTP.
pub async fn run(cfg: Arc<Config>, channels: ChannelManager) -> anyhow::Result<()> {
    let addr = cfg.dash_addr()?;
    let live_root = cfg.cache.dir.join("live");
    std::fs::create_dir_all(&live_root)?;

    let state = AppState {
        cache_dir: cfg.cache.dir.clone(),
        channels,
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/channels", get(list_channels))
        .route("/live/{channel}/index.mpd", get(serve_mpd))
        .route("/live/{channel}/{file}", get(serve_media))
        .layer(cors)
        .with_state(state);

    info!("DASH HTTP listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// `GET /channels` — list channels that currently hold an active ingest lease.
async fn list_channels(State(state): State<AppState>) -> Json<ChannelsResponse> {
    let channels = state
        .channels
        .list_active()
        .into_iter()
        .map(|id| ChannelInfo {
            mpd: format!("/live/{id}/index.mpd"),
            id,
        })
        .collect();
    Json(ChannelsResponse { channels })
}

/// Serve `/live/{channel}/index.mpd` with no-cache headers for live playback.
async fn serve_mpd(State(state): State<AppState>, Path(channel): Path<String>) -> Response {
    if !is_safe_channel(&channel) {
        return (StatusCode::BAD_REQUEST, "invalid channel id").into_response();
    }
    let path = state
        .cache_dir
        .join("live")
        .join(&channel)
        .join("index.mpd");
    match tokio::fs::read(&path).await {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/dash+xml")
            .header(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(_) => (StatusCode::NOT_FOUND, "channel offline or mpd missing").into_response(),
    }
}

/// Serve `/live/{channel}/{file}` for init segments and media segments.
async fn serve_media(
    State(state): State<AppState>,
    Path((channel, file)): Path<(String, String)>,
) -> Response {
    if !is_safe_channel(&channel) || !is_safe_file(&file) {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    }
    let path = state.cache_dir.join("live").join(&channel).join(&file);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let content_type = if file.ends_with(".mp4") || file.ends_with(".m4s") {
                "video/mp4"
            } else {
                "application/octet-stream"
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(bytes))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Return true if `channel` is a safe path segment for cache and HTTP URLs.
fn is_safe_channel(channel: &str) -> bool {
    !channel.is_empty()
        && channel.len() <= 128
        && channel
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Return true if `file` is an allowed media/manifest basename (no path traversal).
fn is_safe_file(file: &str) -> bool {
    !file.is_empty()
        && !file.contains("..")
        && !file.contains('/')
        && !file.contains('\\')
        && (file == "init.mp4"
            || file.ends_with(".m4s")
            || file.ends_with(".mp4")
            || file.ends_with(".mpd"))
}
