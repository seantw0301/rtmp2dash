use crate::config::CacheConfig;
use crate::dash::mpd::{self, MpdTrackInfo};
use crate::dash::writer::{clear_channel_dir, PackagerWriter};
use crate::demux::{AccessUnit, AUDIO_TRACK_ID, TIMESCALE, VIDEO_TRACK_ID};
use anyhow::Result;
use std::collections::VecDeque;
use std::path::PathBuf;
use tracing::{debug, info, warn};
use transmux::{CodecConfig, Sample, Segmenter, TrackSpec};

const MAX_PENDING_SAMPLE_BYTES: usize = 64 * 1024 * 1024;

pub struct DashPackager {
    out_dir: PathBuf,
    writer: PackagerWriter,
    segment_duration_secs: f64,
    window_segments: usize,
    video_config: Option<CodecConfig>,
    audio_config: Option<CodecConfig>,
    segmenter: Option<Segmenter>,
    next_segment_number: u64,
    /// Inclusive start of the sliding window (segment numbers on disk).
    window_start: u64,
    /// Preserve startup samples until both AVC and AAC configurations arrive.
    pending_samples: VecDeque<(u32, Sample)>,
    pending_sample_bytes: usize,
    /// CMAF video segments must begin on a random-access sample.
    started_on_video_sync: bool,
    /// Coalesce MPD rewrites to at most once per `drain_ready` batch.
    mpd_dirty: bool,
}

impl DashPackager {
    /// Create a packager for `out_dir`, clearing leftover files from a previous run.
    pub fn new(out_dir: PathBuf, cache: &CacheConfig) -> Result<Self> {
        clear_channel_dir(&out_dir)?;
        let writer = PackagerWriter::spawn(out_dir.clone());

        Ok(Self {
            out_dir,
            writer,
            segment_duration_secs: cache.segment_duration_secs,
            window_segments: cache.window_segments,
            video_config: None,
            audio_config: None,
            segmenter: None,
            next_segment_number: 1,
            window_start: 1,
            pending_samples: VecDeque::new(),
            pending_sample_bytes: 0,
            started_on_video_sync: false,
            mpd_dirty: false,
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
        self.flush_mpd_if_dirty();
        self.writer.shutdown();
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
        self.writer.enqueue("init.mp4", init);
        self.segmenter = Some(segmenter);
        self.replay_pending_samples();
        info!(
            dir = %self.out_dir.display(),
            segment_duration_secs = self.segment_duration_secs,
            "DASH packager started (init.mp4 queued)"
        );
    }

    /// Push one sample into the segmenter and drain any completed media segments.
    fn push_sample(&mut self, track_id: u32, sample: Sample) {
        if self.segmenter.is_none() {
            self.queue_pending_sample(track_id, sample);
            return;
        }
        self.push_ready_sample(track_id, sample);
    }

    fn queue_pending_sample(&mut self, track_id: u32, sample: Sample) {
        self.pending_sample_bytes = self.pending_sample_bytes.saturating_add(sample.data.len());
        self.pending_samples.push_back((track_id, sample));
        while self.pending_sample_bytes > MAX_PENDING_SAMPLE_BYTES {
            let Some((_, dropped)) = self.pending_samples.pop_front() else {
                break;
            };
            self.pending_sample_bytes =
                self.pending_sample_bytes.saturating_sub(dropped.data.len());
            warn!("startup sample buffer exceeded 64 MiB; dropped oldest sample");
        }
    }

    fn replay_pending_samples(&mut self) {
        let first_sync = self
            .pending_samples
            .iter()
            .position(|(track_id, sample)| *track_id == VIDEO_TRACK_ID && sample.is_sync);
        let Some(first_sync) = first_sync else {
            self.pending_samples.clear();
            self.pending_sample_bytes = 0;
            debug!("waiting for first video keyframe after codec configuration");
            return;
        };
        for _ in 0..first_sync {
            self.pending_samples.pop_front();
        }
        let pending = std::mem::take(&mut self.pending_samples);
        self.pending_sample_bytes = 0;
        for (track_id, sample) in pending {
            self.push_ready_sample(track_id, sample);
        }
    }

    fn push_ready_sample(&mut self, track_id: u32, sample: Sample) {
        if !self.started_on_video_sync {
            if track_id != VIDEO_TRACK_ID || !sample.is_sync {
                return;
            }
            self.started_on_video_sync = true;
        }
        let Some(seg) = self.segmenter.as_mut() else {
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
            let number = self.next_segment_number;
            let name = format!("seg_{number}.m4s");
            self.writer.enqueue(&name, media);
            debug!(segment = number, "queued media segment");
            self.next_segment_number = self.next_segment_number.saturating_add(1);
            self.prune_window();
            self.mpd_dirty = true;
        }
        self.flush_mpd_if_dirty();
    }

    /// Drop segment files that fall outside the configured sliding window.
    fn prune_window(&mut self) {
        while self.next_segment_number.saturating_sub(self.window_start)
            > self.window_segments as u64
        {
            let old = self.window_start;
            self.window_start = self.window_start.saturating_add(1);
            self.writer.delete(&format!("seg_{old}.m4s"));
        }
    }

    /// Render and enqueue `index.mpd` when the timeline changed since the last write.
    fn flush_mpd_if_dirty(&mut self) {
        if !self.mpd_dirty {
            return;
        }
        self.mpd_dirty = false;
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
        self.writer.enqueue("index.mpd", xml.into_bytes());
    }
}

impl Drop for DashPackager {
    fn drop(&mut self) {
        self.flush_mpd_if_dirty();
        self.writer.shutdown();
    }
}
