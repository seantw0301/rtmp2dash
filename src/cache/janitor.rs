use crate::config::Config;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{debug, info, warn};

/// Cap deletions per channel per sweep so one large directory cannot starve others.
const MAX_DELETIONS_PER_CHANNEL_SWEEP: usize = 100;

/// Periodically delete stale **segment** files under `cache/live/`.
///
/// Preserves `init.mp4` while a live window still has segments. When every
/// `seg_*.m4s` has expired but `index.mpd` remains, the MPD (and orphan init)
/// are removed so HTTP does not keep serving a ghost timeline after ingest dies.
pub async fn run(cfg: Arc<Config>) {
    let interval = Duration::from_secs(cfg.cache.cleanup_interval_secs.max(1));
    let ttl = Duration::from_secs(cfg.cache.effective_ttl_secs());
    let live_root = cfg.cache.dir.join("live");

    info!(
        root = %live_root.display(),
        ttl_secs = ttl.as_secs(),
        interval_secs = interval.as_secs(),
        max_deletions_per_channel_sweep = MAX_DELETIONS_PER_CHANNEL_SWEEP,
        "cache janitor started (drops orphan index.mpd when no segments remain)"
    );

    loop {
        match tokio::task::spawn_blocking({
            let live_root = live_root.clone();
            move || sweep(&live_root, ttl)
        })
        .await
        {
            Ok(_) => {}
            Err(err) => warn!("cache janitor task join error: {err:#}"),
        }
        tokio::time::sleep(interval).await;
    }
}

/// Scan channel dirs under `live_root` and delete expired media segments (batched).
fn sweep(live_root: &Path, ttl: Duration) -> usize {
    if !live_root.is_dir() {
        return 0;
    }

    let now = SystemTime::now();
    let mut removed = 0usize;

    let Ok(channels) = fs::read_dir(live_root) else {
        return 0;
    };

    for entry in channels.flatten() {
        let channel_dir = entry.path();
        if !channel_dir.is_dir() {
            continue;
        }
        removed += sweep_channel(&channel_dir, now, ttl, MAX_DELETIONS_PER_CHANNEL_SWEEP);
    }

    if removed > 0 {
        debug!(removed, "cache janitor cleaned old segment files");
    }
    removed
}

/// Delete expired `seg_*.m4s` (and stray `.tmp`) files in one channel directory.
/// If no segments remain and `index.mpd` is older than `ttl`, drop the ghost MPD
/// (and `init.mp4`) so clients get 404 instead of a dead SegmentTimeline.
fn sweep_channel(channel_dir: &Path, now: SystemTime, ttl: Duration, mut budget: usize) -> usize {
    if budget == 0 {
        return 0;
    }
    let Ok(entries) = fs::read_dir(channel_dir) else {
        return 0;
    };
    let mut removed = 0usize;
    let mut segment_remaining = 0usize;
    let mut mpd_path: Option<PathBuf> = None;
    let mut init_path: Option<PathBuf> = None;
    let mut expired_segs: Vec<PathBuf> = Vec::new();
    let mut expired_tmps: Vec<PathBuf> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if name == "index.mpd" {
            mpd_path = Some(path);
            continue;
        }
        if name == "init.mp4" {
            init_path = Some(path);
            continue;
        }

        // Remove only stale atomic-write temps. Packager writes and renames these
        // concurrently, so unconditional deletion can race an active MPD/segment write.
        if name.ends_with(".tmp") {
            if file_is_expired(&path, now, Duration::from_secs(60)) {
                expired_tmps.push(path);
            }
            continue;
        }

        let is_segment = name.starts_with("seg_") && name.ends_with(".m4s");
        if !is_segment {
            continue;
        }

        if file_is_expired(&path, now, ttl) {
            expired_segs.push(path);
        } else {
            segment_remaining += 1;
        }
    }

    for path in expired_tmps {
        if budget == 0 {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            removed += 1;
            budget -= 1;
        }
    }

    let mut expired_left = expired_segs.len();
    for path in expired_segs {
        if budget == 0 {
            break;
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                debug!(file = %path.display(), "deleted expired segment");
                removed += 1;
                budget -= 1;
                expired_left = expired_left.saturating_sub(1);
            }
            Err(err) => warn!(file = %path.display(), %err, "failed to delete segment"),
        }
    }

    // Only treat as orphan when nothing live remains and every expired seg was
    // actually removed (budget may truncate a large directory).
    if segment_remaining == 0 && expired_left == 0 {
        if let Some(mpd) = mpd_path {
            if file_is_expired(&mpd, now, ttl) && budget > 0 {
                match fs::remove_file(&mpd) {
                    Ok(()) => {
                        debug!(file = %mpd.display(), "deleted orphan index.mpd (no segments)");
                        removed += 1;
                        budget -= 1;
                        if let Some(init) = init_path {
                            if budget > 0 && fs::remove_file(&init).is_ok() {
                                debug!(file = %init.display(), "deleted orphan init.mp4");
                                removed += 1;
                            }
                        }
                    }
                    Err(err) => warn!(file = %mpd.display(), %err, "failed to delete orphan mpd"),
                }
            }
        }
    }

    removed
}

/// Return true if the file's mtime is older than `ttl` relative to `now`.
fn file_is_expired(path: &Path, now: SystemTime, ttl: Duration) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    now.duration_since(modified)
        .map(|age| age > ttl)
        .unwrap_or(false)
}

/// Best-effort delete of an entire channel cache directory (e.g. after stream ends).
#[allow(dead_code)]
pub fn remove_channel_dir(cache_dir: &Path, channel: &str) {
    let dir: PathBuf = cache_dir.join("live").join(channel);
    if dir.is_dir() {
        let _ = fs::remove_dir_all(&dir);
    }
}
