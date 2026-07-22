use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub rtmp: RtmpConfig,
    pub dash: DashConfig,
    pub cache: CacheConfig,
    /// Remote RTMP sources to pull (runs in parallel with local publish ingest).
    #[serde(default)]
    pub pull: Vec<PullSource>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RtmpConfig {
    pub listen: String,
    pub port: u16,
    #[serde(default = "default_app")]
    pub app: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DashConfig {
    pub listen: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    pub dir: PathBuf,
    #[serde(default = "default_segment_duration")]
    pub segment_duration_secs: f64,
    #[serde(default = "default_window_segments")]
    pub window_segments: usize,
    /// Delete cache files older than this many seconds (by mtime).
    /// Default: `window_segments * segment_duration_secs * 2` (at least 30s).
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    /// How often the background janitor scans the cache (seconds). Default 10.
    #[serde(default = "default_cleanup_interval_secs")]
    pub cleanup_interval_secs: u64,
    /// Opt-in re-encode for upstream GOP that cannot meet 2s keyframe cuts.
    /// `off` (default) = passthrough; `force_2s_gop` = ffmpeg/libx264 keyint=fps×2.
    #[serde(default = "default_reencode_profile")]
    pub reencode_profile: ReencodeProfile,
    /// Max |audio_tfdt − video_tfdt| (ms) before the packager rotates the CMAF
    /// generation. Default 500ms (normal A/V lead is ≪ 100ms).
    #[serde(default = "default_av_tfdt_max_skew_ms")]
    pub av_tfdt_max_skew_ms: u64,
    /// How often the publish session re-checks the latest on-disk segment for
    /// A/V `tfdt` skew (seconds). Segment drain also checks every fragment.
    #[serde(default = "default_av_tfdt_check_interval_secs")]
    pub av_tfdt_check_interval_secs: u64,
}

/// Ingest re-encode profile (Phase 4). Default remains passthrough.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReencodeProfile {
    #[default]
    Off,
    Force2sGop,
}

fn default_reencode_profile() -> ReencodeProfile {
    ReencodeProfile::Off
}

fn default_av_tfdt_max_skew_ms() -> u64 {
    500
}

fn default_av_tfdt_check_interval_secs() -> u64 {
    2
}

impl CacheConfig {
    /// Effective TTL used by the janitor (never below the live window duration).
    pub fn effective_ttl_secs(&self) -> u64 {
        let window_secs = (self.window_segments as f64 * self.segment_duration_secs).ceil() as u64;
        let floor = window_secs.saturating_mul(2).max(30);
        match self.ttl_secs {
            // Keep a full extra advertised window as grace for clients that are
            // still consuming a previously fetched MPD.
            Some(t) => t.max(floor),
            None => floor,
        }
    }
}

/// Pull one remote RTMP URL and publish DASH under `/live/<channel>/index.mpd`.
#[derive(Debug, Clone, Deserialize)]
pub struct PullSource {
    /// Source RTMP URL, e.g. `rtmp://origin.example.com:1935/live/stream1`
    pub url: String,
    /// Output channel id (DASH path segment)
    pub channel: String,
    /// Seconds to wait before reconnecting after disconnect (default 3)
    #[serde(default = "default_reconnect_secs")]
    pub reconnect_secs: u64,
}

#[derive(Debug, Clone)]
pub struct ParsedRtmpUrl {
    pub host: String,
    pub port: u16,
    pub app: String,
    pub stream_key: String,
    pub tc_url: String,
}

impl PullSource {
    /// Parse this source's RTMP URL into host/port/app/stream_key fields.
    pub fn parse_url(&self) -> Result<ParsedRtmpUrl> {
        parse_rtmp_url(&self.url)
    }
}

/// Parse `rtmp://host[:port]/app/stream_key` (stream_key may contain `/`).
pub fn parse_rtmp_url(url: &str) -> Result<ParsedRtmpUrl> {
    let rest = url
        .strip_prefix("rtmp://")
        .or_else(|| url.strip_prefix("RTMP://"))
        .with_context(|| format!("URL must start with rtmp:// (got {url})"))?;

    let (authority, path) = rest
        .split_once('/')
        .with_context(|| format!("RTMP URL missing path: {url}"))?;
    if path.is_empty() {
        bail!("RTMP URL path empty: {url}");
    }

    let (host, port) = if let Some((h, p)) = authority.rsplit_once(':') {
        // IPv6 in brackets not supported in this simple parser
        let port: u16 = p
            .parse()
            .with_context(|| format!("invalid RTMP port in {url}"))?;
        (h.to_string(), port)
    } else {
        (authority.to_string(), 1935)
    };

    let (app, stream_key) = path
        .split_once('/')
        .with_context(|| format!("RTMP URL needs /app/stream_key: {url}"))?;
    if app.is_empty() || stream_key.is_empty() {
        bail!("RTMP URL app/stream_key empty: {url}");
    }

    let tc_url = format!("rtmp://{host}:{port}/{app}");
    Ok(ParsedRtmpUrl {
        host,
        port,
        app: app.to_string(),
        stream_key: stream_key.to_string(),
        tc_url,
    })
}

/// Default RTMP application name when omitted from config.
fn default_app() -> String {
    "live".to_string()
}

/// Default DASH segment duration in seconds.
fn default_segment_duration() -> f64 {
    2.0
}

/// Default number of segments kept in the live sliding window (~3 min at 2s/seg).
fn default_window_segments() -> usize {
    90
}

/// Default interval between cache janitor sweeps, in seconds.
fn default_cleanup_interval_secs() -> u64 {
    10
}

/// Default pull reconnect delay after disconnect, in seconds.
fn default_reconnect_secs() -> u64 {
    3
}

impl Config {
    /// Load and validate configuration from a YAML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let cfg: Config = serde_yaml::from_str(&raw)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject invalid listen/cache/pull settings before the process starts serving.
    fn validate(&self) -> Result<()> {
        if self.rtmp.app.trim().is_empty() {
            bail!("rtmp.app must not be empty");
        }
        if !(self.cache.segment_duration_secs.is_finite() && self.cache.segment_duration_secs > 0.0)
        {
            bail!("cache.segment_duration_secs must be > 0");
        }
        if self.cache.window_segments == 0 {
            bail!("cache.window_segments must be >= 1");
        }
        if self.cache.cleanup_interval_secs == 0 {
            bail!("cache.cleanup_interval_secs must be >= 1");
        }
        if self.cache.av_tfdt_max_skew_ms == 0 {
            bail!("cache.av_tfdt_max_skew_ms must be >= 1");
        }
        if self.cache.av_tfdt_check_interval_secs == 0 {
            bail!("cache.av_tfdt_check_interval_secs must be >= 1");
        }
        if let Some(ttl) = self.cache.ttl_secs {
            if ttl == 0 {
                bail!("cache.ttl_secs must be >= 1 when set");
            }
        }
        if matches!(self.cache.reencode_profile, ReencodeProfile::Force2sGop) {
            tracing::warn!(
                "cache.reencode_profile=force_2s_gop is configured (opt-in Phase 4); \
                 passthrough remains preferred when upstream GOP can be fixed — see doc/gop_origin_runbook.md"
            );
        }

        let mut seen = std::collections::HashSet::new();
        for (i, src) in self.pull.iter().enumerate() {
            if src.channel.trim().is_empty() {
                bail!("pull[{i}].channel must not be empty");
            }
            if !is_safe_channel(&src.channel) {
                bail!("pull[{i}].channel has invalid characters: {}", src.channel);
            }
            if !seen.insert(src.channel.clone()) {
                bail!("duplicate pull channel '{}'", src.channel);
            }
            // Validate URL parses
            src.parse_url()
                .with_context(|| format!("pull[{i}].url invalid"))?;
        }
        Ok(())
    }

    /// Resolve the RTMP listen address from config.
    pub fn rtmp_addr(&self) -> Result<SocketAddr> {
        format!("{}:{}", self.rtmp.listen, self.rtmp.port)
            .parse()
            .context("invalid rtmp.listen/port")
    }

    /// Resolve the DASH HTTP listen address from config.
    pub fn dash_addr(&self) -> Result<SocketAddr> {
        format!("{}:{}", self.dash.listen, self.dash.port)
            .parse()
            .context("invalid dash.listen/port")
    }

    /// Cache directory path for a given channel id (`cache/live/<channel_id>`).
    pub fn channel_dir(&self, channel_id: &str) -> PathBuf {
        self.cache.dir.join("live").join(channel_id)
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
