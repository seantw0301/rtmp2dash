use crate::config::CacheConfig;
use crate::dash::mpd::{self, MpdTrackInfo};
use crate::demux::{AccessUnit, AUDIO_TRACK_ID, TIMESCALE, VIDEO_TRACK_ID};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};
use transmux::{CodecConfig, Sample, Segmenter, TrackSpec};

pub struct DashPackager {
    out_dir: PathBuf,
    segment_duration_secs: f64,
    window_segments: usize,
    video_config: Option<CodecConfig>,
    audio_config: Option<CodecConfig>,
    segmenter: Option<Segmenter>,
    next_segment_number: u64,
    /// Inclusive start of the sliding window (segment numbers on disk).
    window_start: u64,
}

impl DashPackager {
    /// Create a packager for `out_dir`, clearing leftover files from a previous run.
    pub fn new(out_dir: PathBuf, cache: &CacheConfig) -> Result<Self> {
        fs::create_dir_all(&out_dir)
            .with_context(|| format!("create channel dir {}", out_dir.display()))?;
        // Clear previous run artifacts (best-effort).
        if let Ok(entries) = fs::read_dir(&out_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    let _ = fs::remove_file(&path);
                }
            }
        }

        Ok(Self {
            out_dir,
            segment_duration_secs: cache.segment_duration_secs,
            window_segments: cache.window_segments,
            video_config: None,
            audio_config: None,
            segmenter: None,
            next_segment_number: 1,
            window_start: 1,
        })
    }

    /// Ingest one access unit (codec config or sample) into the live DASH pipeline.
    pub fn handle_au(&mut self, au: AccessUnit) -> Result<()> {
        match au {
            AccessUnit::VideoConfig { config } => {
                // Mid-stream SPS refresh: keep first config used for the segmenter.
                if self.video_config.is_none() {
                    self.video_config = Some(config);
                    self.maybe_start_segmenter();
                }
            }
            AccessUnit::AudioConfig { config, .. } => {
                if self.audio_config.is_none() {
                    self.audio_config = Some(config);
                    self.maybe_start_segmenter();
                }
            }
            AccessUnit::VideoSample(sample) => self.push_sample(VIDEO_TRACK_ID, sample),
            AccessUnit::AudioSample(sample) => self.push_sample(AUDIO_TRACK_ID, sample),
        }
        Ok(())
    }

    /// Flush the segmenter and write any remaining media segments plus MPD.
    pub fn finish(&mut self) {
        if let Some(seg) = self.segmenter.as_mut() {
            if let Err(err) = seg.flush() {
                warn!("segmenter flush: {err}");
            }
            self.drain_ready();
        }
    }

    /// Start the CMAF segmenter once both video and audio codec configs are known.
    fn maybe_start_segmenter(&mut self) {
        if self.segmenter.is_some() {
            return;
        }
        let (Some(video), Some(audio)) = (self.video_config.clone(), self.audio_config.clone())
        else {
            return;
        };

        let tracks = vec![
            TrackSpec::new(VIDEO_TRACK_ID, TIMESCALE, video),
            TrackSpec::new(AUDIO_TRACK_ID, TIMESCALE, audio),
        ];

        let segmenter = match Segmenter::new(tracks, TIMESCALE, self.segment_duration_secs) {
            Ok(s) => s,
            Err(err) => {
                warn!("create segmenter failed: {err}");
                return;
            }
        };
        let init = match segmenter.init_segment() {
            Ok(b) => b,
            Err(err) => {
                warn!("build init failed: {err}");
                return;
            }
        };
        if let Err(err) = self.write_bytes("init.mp4", &init) {
            warn!("write init.mp4 failed: {err:#}");
            return;
        }
        self.segmenter = Some(segmenter);
        self.write_mpd();
        info!(
            dir = %self.out_dir.display(),
            segment_duration_secs = self.segment_duration_secs,
            "DASH packager started (init.mp4 written)"
        );
    }

    /// Push one sample into the segmenter and drain any completed media segments.
    fn push_sample(&mut self, track_id: u32, sample: Sample) {
        let Some(seg) = self.segmenter.as_mut() else {
            debug!("dropping sample before segmenter ready (track={track_id})");
            return;
        };
        if let Err(err) = seg.push(track_id, sample) {
            warn!("segmenter push track {track_id}: {err}");
            return;
        }
        self.drain_ready();
    }

    /// Write completed media segments to disk, prune the sliding window, and refresh the MPD.
    fn drain_ready(&mut self) {
        let ready = self
            .segmenter
            .as_mut()
            .map(|s| s.take_ready())
            .unwrap_or_default();
        for media in ready {
            let name = format!("seg_{}.m4s", self.next_segment_number);
            if let Err(err) = self.write_bytes(&name, &media) {
                warn!(segment = self.next_segment_number, "write segment failed: {err:#}");
                // Still advance number to avoid colliding filenames on next success.
            } else {
                info!(segment = self.next_segment_number, "wrote media segment");
            }
            self.next_segment_number = self.next_segment_number.saturating_add(1);
            self.prune_window();
            self.write_mpd();
        }
    }

    /// Drop segment files that fall outside the configured sliding window.
    fn prune_window(&mut self) {
        while self.next_segment_number.saturating_sub(self.window_start)
            > self.window_segments as u64
        {
            let old = self.window_start;
            let path = self.out_dir.join(format!("seg_{old}.m4s"));
            let _ = fs::remove_file(&path);
            self.window_start = self.window_start.saturating_add(1);
        }
        self.prune_orphaned_segments();
    }

    /// Remove on-disk `seg_*.m4s` files older than the current window start.
    fn prune_orphaned_segments(&self) {
        let Ok(entries) = fs::read_dir(&self.out_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(num) = name
                .strip_prefix("seg_")
                .and_then(|s| s.strip_suffix(".m4s"))
                .and_then(|s| s.parse::<u64>().ok())
            else {
                continue;
            };
            if num < self.window_start {
                let _ = fs::remove_file(&path);
            }
        }
    }

    /// Render and atomically write `index.mpd` for the current sliding window.
    fn write_mpd(&self) {
        let (Some(video_cfg), Some(audio_cfg)) =
            (self.video_config.as_ref(), self.audio_config.as_ref())
        else {
            return;
        };
        let video = MpdTrackInfo::from_video(video_cfg);
        let audio = MpdTrackInfo::from_audio(audio_cfg);
        let latest = self.next_segment_number.saturating_sub(1);
        let xml = mpd::render_live_mpd(
            self.segment_duration_secs,
            self.window_segments,
            self.window_start,
            latest.max(self.window_start.saturating_sub(1)),
            &video,
            Some(&audio),
        );
        if let Err(err) = self.write_bytes("index.mpd", xml.as_bytes()) {
            warn!("write index.mpd failed: {err:#}");
        }
    }

    /// Atomically write `data` to a file named `name` under the channel output directory.
    fn write_bytes(&self, name: &str, data: &[u8]) -> Result<()> {
        let path = self.out_dir.join(name);
        atomic_write(&path, data).with_context(|| format!("write {}", path.display()))
    }
}

/// Write `data` via a temp file then rename, so readers never see a partial file.
fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err.into())
        }
    }
}
