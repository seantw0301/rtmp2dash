use crate::config::CacheConfig;
use crate::dash::fmp4_duration::{first_tfdt_base_time, first_traf_duration_ticks};
use crate::dash::mpd::{self, MpdTrackInfo, TimelineEntry};
use crate::dash::writer::{clear_channel_dir, PackagerWriter};
use crate::demux::{AccessUnit, AUDIO_TRACK_ID, TIMESCALE, VIDEO_TRACK_ID};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};
use transmux::{CodecConfig, Sample, Segmenter, TrackSpec};

const MAX_PENDING_SAMPLE_BYTES: usize = 64 * 1024 * 1024;
/// Keep segments a bit longer on disk than advertised so late clients don't 404.
const PRUNE_GRACE_SEGMENTS: u64 = 2;

pub struct DashPackager {
    out_dir: PathBuf,
    writer: PackagerWriter,
    /// Configured cut target (fallback when a segment's media duration cannot be parsed).
    segment_duration_secs: f64,
    window_segments: usize,
    video_config: Option<CodecConfig>,
    audio_config: Option<CodecConfig>,
    segmenter: Option<Segmenter>,
    next_segment_number: u64,
    /// Inclusive start of the on-disk sliding window.
    window_start: u64,
    /// Actual media durations for segments still in the advertised window.
    timeline: VecDeque<TimelineEntry>,
    /// Media start time (ticks) expected for the next drained segment; fallback
    /// when a fragment's tfdt cannot be parsed.
    next_start_ticks: u64,
    /// AvailabilityStartTime anchored ONCE per generation. Re-anchoring on every
    /// MPD update shifts the media-time → wall-clock mapping under the player,
    /// which eventually stalls playback at the end of the first window.
    availability_start_time: Option<DateTime<Utc>>,
    /// Preserve startup samples until both AVC and AAC configurations arrive.
    pending_samples: VecDeque<(u32, Sample)>,
    pending_sample_bytes: usize,
    /// CMAF video segments must begin on a random-access sample.
    started_on_video_sync: bool,
    /// Coalesce MPD rewrites to at most once per `drain_ready` batch.
    mpd_dirty: bool,
}

impl DashPackager {
    /// Create a packager for a fresh publish: clear leftover cache files and start at seg 1.
    pub fn new(out_dir: PathBuf, cache: &CacheConfig) -> Result<Self> {
        Self::create(out_dir, cache, /*clear=*/ true)
    }

    /// Create a packager after process restart for RTMP pull: continue numbering but wipe
    /// leftover segments/MPD. A brand-new Segmenter always starts tfdt at 0, so old media
    /// files are never compatible with a new init — keeping them would desync playback.
    pub fn resume(out_dir: PathBuf, cache: &CacheConfig) -> Result<Self> {
        Self::create(out_dir, cache, /*clear=*/ false)
    }

    fn create(out_dir: PathBuf, cache: &CacheConfig, clear: bool) -> Result<Self> {
        let next_segment_number = if clear {
            clear_channel_dir(&out_dir)?;
            1
        } else {
            fs::create_dir_all(&out_dir)?;
            let next = scan_next_segment_number(&out_dir);
            // Drop incompatible leftovers from the previous process; numbering continues.
            wipe_channel_media(&out_dir)?;
            info!(
                dir = %out_dir.display(),
                next_segment = next,
                "DASH packager resume: wiped prior media, continuing numbering"
            );
            next
        };
        let writer = PackagerWriter::spawn(out_dir.clone());

        Ok(Self {
            out_dir,
            writer,
            segment_duration_secs: cache.segment_duration_secs,
            window_segments: cache.window_segments,
            video_config: None,
            audio_config: None,
            segmenter: None,
            next_segment_number,
            window_start: next_segment_number,
            timeline: VecDeque::new(),
            next_start_ticks: 0,
            availability_start_time: None,
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
                match &self.video_config {
                    None => {
                        self.video_config = Some(config);
                        self.maybe_start_segmenter();
                    }
                    Some(prev) if codec_config_eq(prev, &config) => {
                        // Encoder periodically re-sends SPS/PPS — ignore identical copies.
                    }
                    Some(_) => {
                        info!(
                            dir = %self.out_dir.display(),
                            "video codec config changed; rotating DASH generation"
                        );
                        self.video_config = Some(config);
                        self.rotate();
                    }
                }
            }
            AccessUnit::AudioConfig { config, .. } => {
                match &self.audio_config {
                    None => {
                        self.audio_config = Some(config);
                        self.maybe_start_segmenter();
                    }
                    Some(prev) if codec_config_eq(prev, &config) => {}
                    Some(_) => {
                        info!(
                            dir = %self.out_dir.display(),
                            "audio codec config changed; rotating DASH generation"
                        );
                        self.audio_config = Some(config);
                        self.rotate();
                    }
                }
            }
            AccessUnit::VideoSample(sample) => self.push_sample(VIDEO_TRACK_ID, sample),
            AccessUnit::AudioSample(sample) => self.push_sample(AUDIO_TRACK_ID, sample),
        }
        Ok(())
    }

    /// Flush buffered media into the current generation without shutting the writer.
    /// Used across pull reconnects so the same Segmenter (tfdt / moof seq) stays alive.
    pub fn flush_tail(&mut self) {
        if let Some(seg) = self.segmenter.as_mut() {
            if let Err(err) = seg.flush() {
                warn!("segmenter flush: {err}");
            }
            self.drain_ready();
        }
        self.flush_mpd_if_dirty();
    }

    /// Flush remaining media segments / MPD and wait for all disk I/O to finish.
    pub async fn finish(&mut self) {
        self.flush_tail();
        self.writer.shutdown_and_drain().await;
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

        if let Some(segmenter) = Self::build_segmenter(video, audio, self.segment_duration_secs) {
            self.install_segmenter(segmenter);
        }
    }

    /// Tear down the current generation and start a new one with the current codec configs.
    /// Segment numbers continue; old media + MPD are removed so clients never pair a new
    /// init.mp4 with leftover segments.
    fn rotate(&mut self) {
        if let Some(seg) = self.segmenter.as_mut() {
            let _ = seg.flush();
            // Discard remaining samples of the old generation — they do not match the new init.
            let _ = seg.take_ready();
        }
        self.segmenter = None;
        self.pending_samples.clear();
        self.pending_sample_bytes = 0;
        self.started_on_video_sync = false;
        self.mpd_dirty = false;
        self.availability_start_time = None;

        self.purge_generation_files();
        self.timeline.clear();
        self.next_start_ticks = 0;
        self.window_start = self.next_segment_number;

        let (Some(video), Some(audio)) = (self.video_config.clone(), self.audio_config.clone())
        else {
            return;
        };
        if let Some(segmenter) = Self::build_segmenter(video, audio, self.segment_duration_secs) {
            self.install_segmenter(segmenter);
        }
    }

    fn build_segmenter(
        video: CodecConfig,
        audio: CodecConfig,
        segment_duration_secs: f64,
    ) -> Option<Segmenter> {
        let tracks = vec![
            TrackSpec::new(VIDEO_TRACK_ID, TIMESCALE, video),
            TrackSpec::new(AUDIO_TRACK_ID, TIMESCALE, audio),
        ];
        match Segmenter::new(tracks, TIMESCALE, segment_duration_secs) {
            Ok(s) => Some(s),
            Err(err) => {
                warn!("create segmenter failed: {err}");
                None
            }
        }
    }

    fn install_segmenter(&mut self, segmenter: Segmenter) {
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
            window_segments = self.window_segments,
            next_segment = self.next_segment_number,
            "DASH packager started (init.mp4 queued)"
        );
    }

    /// Delete every on-disk media segment in the advertised window plus the live MPD.
    fn purge_generation_files(&mut self) {
        for entry in &self.timeline {
            self.writer.delete(&format!("seg_{}.m4s", entry.number));
        }
        // Also remove any grace/orphan segments that may sit on disk outside timeline.
        let keep_from = self.window_start;
        let keep_to = self.next_segment_number.saturating_sub(1);
        if keep_to >= keep_from {
            for n in keep_from..=keep_to {
                self.writer.delete(&format!("seg_{n}.m4s"));
            }
        }
        self.writer.delete("index.mpd");
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

    /// Write completed media segments to disk, refresh the MPD, then prune old files.
    fn drain_ready(&mut self) {
        let ready = self
            .segmenter
            .as_mut()
            .map(|s| s.take_ready())
            .unwrap_or_default();
        let fallback_ticks = (self.segment_duration_secs * 1000.0).round().max(1.0) as u64;
        for media in ready {
            let number = self.next_segment_number;
            let duration_ticks = first_traf_duration_ticks(&media).unwrap_or(fallback_ticks).max(1);
            let start_ticks = first_tfdt_base_time(&media).unwrap_or(self.next_start_ticks);
            self.next_start_ticks = start_ticks.saturating_add(duration_ticks);
            let name = format!("seg_{number}.m4s");
            self.writer.enqueue(&name, media);
            self.timeline.push_back(TimelineEntry {
                number,
                start_ticks,
                duration_ticks,
            });
            debug!(segment = number, start_ticks, duration_ticks, "queued media segment");
            self.next_segment_number = self.next_segment_number.saturating_add(1);
            self.mpd_dirty = true;
        }
        self.prune_window();
        self.flush_mpd_if_dirty();
    }

    /// Drop segment files that fall outside the on-disk sliding window (+ grace).
    fn prune_window(&mut self) {
        let keep = self.window_segments as u64 + PRUNE_GRACE_SEGMENTS;
        while self.next_segment_number.saturating_sub(self.window_start) > keep {
            let old = self.window_start;
            self.window_start = self.window_start.saturating_add(1);
            self.writer.delete(&format!("seg_{old}.m4s"));
            while self
                .timeline
                .front()
                .is_some_and(|e| e.number < self.window_start)
            {
                self.timeline.pop_front();
            }
            self.mpd_dirty = true;
        }
    }

    /// Render and enqueue `index.mpd` from actual per-segment media durations.
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
        if self.timeline.is_empty() {
            return;
        }
        let entries: Vec<TimelineEntry> = self.timeline.iter().copied().collect();
        // Anchor AST exactly once per generation, then hold it constant so the
        // player's mapping from media time to wall clock never shifts.
        let ast = *self
            .availability_start_time
            .get_or_insert_with(|| mpd::availability_start_for_live_edge(Utc::now(), &entries));
        let video = MpdTrackInfo::from_video(video_cfg);
        let audio = MpdTrackInfo::from_audio(audio_cfg);
        let xml = mpd::render_live_mpd(&entries, ast, &video, Some(&audio));
        self.writer.enqueue("index.mpd", xml.into_bytes());
    }
}

impl Drop for DashPackager {
    fn drop(&mut self) {
        self.flush_mpd_if_dirty();
        self.writer.shutdown();
    }
}

/// Compare codec configs by their essential decoder parameters.
/// `CodecConfig` itself is not `PartialEq`, so we match the variants we actually ingest.
fn codec_config_eq(a: &CodecConfig, b: &CodecConfig) -> bool {
    match (a, b) {
        (
            CodecConfig::Avc {
                config: ca,
                width: wa,
                height: ha,
            },
            CodecConfig::Avc {
                config: cb,
                width: wb,
                height: hb,
            },
        ) => ca == cb && wa == wb && ha == hb,
        (
            CodecConfig::Aac {
                esds: ea,
                channel_count: ca,
                sample_rate: ra,
                sample_size: sa,
            },
            CodecConfig::Aac {
                esds: eb,
                channel_count: cb,
                sample_rate: rb,
                sample_size: sb,
            },
        ) => ea == eb && ca == cb && ra == rb && sa == sb,
        _ => false,
    }
}

/// Highest remaining `seg_N.m4s` number + 1, or 1 when the directory is empty.
fn scan_next_segment_number(out_dir: &Path) -> u64 {
    let mut max_seg = 0u64;
    if let Ok(entries) = fs::read_dir(out_dir) {
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if let Some(number) = parse_seg_number(&name) {
                max_seg = max_seg.max(number);
            }
        }
    }
    max_seg.saturating_add(1).max(1)
}

/// Remove leftover media from a previous process (segments + stale MPD). Keeps nothing
/// that could be paired with a freshly-created Segmenter (tfdt always restarts at 0).
fn wipe_channel_media(out_dir: &Path) -> Result<()> {
    if let Ok(entries) = fs::read_dir(out_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let is_seg = name.starts_with("seg_") && name.ends_with(".m4s");
            let is_mpd = name == "index.mpd";
            let is_init = name == "init.mp4";
            let is_tmp = name.ends_with(".tmp");
            if is_seg || is_mpd || is_init || is_tmp {
                let _ = fs::remove_file(&path);
            }
        }
    }
    Ok(())
}

fn parse_seg_number(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("seg_")?.strip_suffix(".m4s")?;
    rest.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use transmux::{
        AVCConfigurationBox, AVCDecoderConfigurationRecord, EsdsBox,
        ESDescriptor, DecoderConfigDescriptor, DecoderSpecificInfo, ObjectTypeIndication,
        SLConfigDescriptor, StreamType,
    };

    fn fake_avc(profile: u8, width: u16, height: u16) -> CodecConfig {
        // Minimal avcC with distinct profile so equality tests can differ.
        let record = AVCDecoderConfigurationRecord {
            configuration_version: 1,
            profile_indication: profile,
            profile_compatibility: 0,
            level_indication: 0x1F,
            length_size_minus_one: 3,
            sps: vec![],
            pps: vec![],
            chroma_format: None,
            bit_depth_luma_minus8: None,
            bit_depth_chroma_minus8: None,
            sps_ext: Vec::new(),
        };
        CodecConfig::Avc {
            config: AVCConfigurationBox::new(record),
            width,
            height,
        }
    }

    fn fake_aac(sample_rate: u32) -> CodecConfig {
        // ASC bytes are opaque for equality — distinct rates produce distinct boxes only if
        // we also vary the ASC payload; use sample_rate field for PartialEq via our helper.
        let asc = vec![0x12, 0x10];
        let esds = EsdsBox::new(ESDescriptor {
            es_id: 1,
            stream_dependence_flag: false,
            url_flag: false,
            ocr_stream_flag: false,
            stream_priority: 0,
            depends_on_es_id: None,
            url: None,
            ocr_es_id: None,
            decoder_config: Some(DecoderConfigDescriptor {
                object_type_indication: ObjectTypeIndication(0x40),
                stream_type: StreamType(0x05),
                up_stream: false,
                buffer_size_db: 0,
                max_bitrate: 0,
                avg_bitrate: 0,
                decoder_specific_info: Some(DecoderSpecificInfo { data: asc }),
            }),
            sl_config: Some(SLConfigDescriptor {
                body: vec![0x02],
            }),
        });
        CodecConfig::Aac {
            esds,
            channel_count: 2,
            sample_rate,
            sample_size: 16,
        }
    }

    #[test]
    fn parse_seg_number_reads_media_names() {
        assert_eq!(parse_seg_number("seg_42.m4s"), Some(42));
        assert_eq!(parse_seg_number("init.mp4"), None);
    }

    #[test]
    fn scan_next_continues_after_existing_segments() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("seg_10.m4s"), b"a").unwrap();
        fs::write(dir.path().join("seg_11.m4s"), b"b").unwrap();
        assert_eq!(scan_next_segment_number(dir.path()), 12);
    }

    #[test]
    fn resume_wipes_old_media_and_continues_numbering() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("seg_10.m4s"), b"a").unwrap();
        fs::write(dir.path().join("seg_11.m4s"), b"b").unwrap();
        fs::write(dir.path().join("index.mpd"), b"stale").unwrap();
        fs::write(dir.path().join("init.mp4"), b"old").unwrap();

        let cache = CacheConfig {
            dir: dir.path().to_path_buf(),
            segment_duration_secs: 2.0,
            window_segments: 90,
            ttl_secs: None,
            cleanup_interval_secs: 10,
        };
        // PackagerWriter::spawn needs a runtime.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let packager = rt.block_on(async {
            let p = DashPackager::resume(dir.path().to_path_buf(), &cache).unwrap();
            // Give wipe a moment; wipe is sync before spawn.
            p
        });
        assert_eq!(packager.next_segment_number, 12);
        assert!(packager.timeline.is_empty());
        assert!(!dir.path().join("seg_10.m4s").exists());
        assert!(!dir.path().join("seg_11.m4s").exists());
        assert!(!dir.path().join("index.mpd").exists());
        assert!(!dir.path().join("init.mp4").exists());
        drop(packager);
    }

    #[test]
    fn codec_config_eq_distinguishes_avc_profiles() {
        let a = fake_avc(0x4D, 1280, 720);
        let b = fake_avc(0x4D, 1280, 720);
        let c = fake_avc(0x42, 1280, 720);
        assert!(codec_config_eq(&a, &b));
        assert!(!codec_config_eq(&a, &c));
    }

    #[test]
    fn codec_config_eq_distinguishes_aac_rates() {
        let a = fake_aac(44100);
        let b = fake_aac(44100);
        let c = fake_aac(48000);
        assert!(codec_config_eq(&a, &b));
        assert!(!codec_config_eq(&a, &c));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rotate_purges_old_segments_and_continues_numbering() {
        let dir = tempdir().unwrap();
        let cache = CacheConfig {
            dir: dir.path().to_path_buf(),
            segment_duration_secs: 2.0,
            window_segments: 90,
            ttl_secs: None,
            cleanup_interval_secs: 10,
        };
        let mut packager = DashPackager::new(dir.path().to_path_buf(), &cache).unwrap();
        packager.video_config = Some(fake_avc(0x4D, 640, 360));
        packager.audio_config = Some(fake_aac(44100));
        packager.next_segment_number = 50;
        packager.window_start = 48;
        packager.timeline.push_back(TimelineEntry {
            number: 48,
            start_ticks: 94_000,
            duration_ticks: 2000,
        });
        packager.timeline.push_back(TimelineEntry {
            number: 49,
            start_ticks: 96_000,
            duration_ticks: 2000,
        });
        // Seed fake on-disk segments matching timeline.
        fs::write(dir.path().join("seg_48.m4s"), b"old48").unwrap();
        fs::write(dir.path().join("seg_49.m4s"), b"old49").unwrap();
        fs::write(dir.path().join("index.mpd"), b"stale-mpd").unwrap();

        packager.video_config = Some(fake_avc(0x42, 640, 360));
        packager.rotate();

        // Drain the delete/init queue.
        packager.finish().await;

        assert_eq!(packager.next_segment_number, 50);
        assert!(packager.timeline.is_empty());
        assert!(!dir.path().join("seg_48.m4s").exists());
        assert!(!dir.path().join("seg_49.m4s").exists());
        assert!(!dir.path().join("index.mpd").exists());
        // New init should have been written by install_segmenter.
        assert!(dir.path().join("init.mp4").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identical_config_resent_does_not_rotate() {
        let dir = tempdir().unwrap();
        let cache = CacheConfig {
            dir: dir.path().to_path_buf(),
            segment_duration_secs: 2.0,
            window_segments: 90,
            ttl_secs: None,
            cleanup_interval_secs: 10,
        };
        let mut packager = DashPackager::new(dir.path().to_path_buf(), &cache).unwrap();
        let video = fake_avc(0x4D, 640, 360);
        let audio = fake_aac(44100);
        packager
            .handle_au(AccessUnit::VideoConfig {
                config: video.clone(),
            })
            .unwrap();
        packager
            .handle_au(AccessUnit::AudioConfig {
                config: audio.clone(),
            })
            .unwrap();
        assert!(packager.segmenter.is_some());
        let next_before = packager.next_segment_number;

        // Seed a fake segment so we can detect accidental purge.
        packager.timeline.push_back(TimelineEntry {
            number: 1,
            start_ticks: 0,
            duration_ticks: 2000,
        });
        packager.next_segment_number = 2;
        packager.window_start = 1;
        fs::write(dir.path().join("seg_1.m4s"), b"keep").unwrap();

        packager
            .handle_au(AccessUnit::VideoConfig { config: video })
            .unwrap();
        packager
            .handle_au(AccessUnit::AudioConfig { config: audio })
            .unwrap();

        assert_eq!(packager.next_segment_number, 2);
        assert_eq!(packager.timeline.len(), 1);
        // Identical re-send must not purge the generation.
        assert!(dir.path().join("seg_1.m4s").exists() || next_before == 1);
        packager.finish().await;
        assert!(dir.path().join("seg_1.m4s").exists());
    }
}
