mod cache;
mod channel;
mod config;
mod dash;
mod debug_ndjson;
mod demux;
mod http;
mod rtmp;

use crate::channel::ChannelManager;
use crate::config::Config;
use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "rtmp2dash",
    about = "RTMP ingest (push + pull) → live MPEG-DASH (H.264 + AAC)"
)]
struct Cli {
    /// Path to YAML config file
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,
}

/// Program entry: load config, start supervised HTTP / RTMP / pull / janitor tasks.
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = Config::load(&cli.config).with_context(|| {
        format!(
            "load config from {} (cwd={})",
            cli.config.display(),
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".into())
        )
    })?;
    let cfg = Arc::new(cfg);

    std::fs::create_dir_all(&cfg.cache.dir)
        .with_context(|| format!("create cache dir {}", cfg.cache.dir.display()))?;

    info!(
        rtmp_push = %format!(
            "rtmp://{}:{}/{}/<channel>",
            cfg.rtmp.listen, cfg.rtmp.port, cfg.rtmp.app
        ),
        dash = %format!(
            "http://{}:{}/live/<channel>/index.mpd",
            cfg.dash.listen, cfg.dash.port
        ),
        pull_sources = cfg.pull.len(),
        cache = %cfg.cache.dir.display(),
        segment_duration_secs = cfg.cache.segment_duration_secs,
        cache_ttl_secs = cfg.cache.effective_ttl_secs(),
        "rtmp2dash starting (push + pull, supervised restart)"
    );
    for src in &cfg.pull {
        info!(
            pull_url = %src.url,
            channel = %src.channel,
            dash = %format!(
                "http://{}:{}/live/{}/index.mpd",
                cfg.dash.listen, cfg.dash.port, src.channel
            ),
            "configured pull source"
        );
    }

    let channels = ChannelManager::new();

    // Each subsystem runs under a restart supervisor so a single failure
    // cannot take down the whole live streaming process.
    let http_cfg = Arc::clone(&cfg);
    let publish_cfg = Arc::clone(&cfg);
    let pull_cfg = Arc::clone(&cfg);
    let janitor_cfg = Arc::clone(&cfg);
    let http_channels = channels.clone();
    let publish_channels = channels.clone();
    let pull_channels = channels.clone();

    let http_task = tokio::spawn(supervise("http", move || {
        let cfg = Arc::clone(&http_cfg);
        let channels = http_channels.clone();
        async move { http::run(cfg, channels).await }
    }));

    let publish_task = tokio::spawn(supervise("rtmp-publish", move || {
        let cfg = Arc::clone(&publish_cfg);
        let channels = publish_channels.clone();
        async move { rtmp::run_publish(cfg, channels).await }
    }));

    let pull_task = tokio::spawn(supervise("rtmp-pull", move || {
        let cfg = Arc::clone(&pull_cfg);
        let channels = pull_channels.clone();
        async move { rtmp::run_pull(cfg, channels).await }
    }));

    let janitor_task = tokio::spawn(supervise("cache-janitor", move || {
        let cfg = Arc::clone(&janitor_cfg);
        async move {
            cache::run(cfg).await;
            Ok(())
        }
    }));

    tokio::select! {
        _ = http_task => warn!("http supervisor ended"),
        _ = publish_task => warn!("publish supervisor ended"),
        _ = pull_task => warn!("pull supervisor ended"),
        _ = janitor_task => warn!("janitor supervisor ended"),
        _ = tokio::signal::ctrl_c() => {
            info!("shutdown signal received");
        }
    }

    Ok(())
}

/// Restart `factory()` forever on error, with backoff.
async fn supervise<F, Fut>(name: &'static str, mut factory: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut backoff = Duration::from_secs(1);
    loop {
        info!(service = name, "service starting");
        match factory().await {
            Ok(()) => {
                warn!(service = name, "service returned Ok (unexpected); restarting");
                backoff = Duration::from_secs(1);
            }
            Err(err) => {
                error!(service = name, "service error: {err:#}; restarting");
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
        tokio::time::sleep(backoff).await;
    }
}
