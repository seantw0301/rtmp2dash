use anyhow::Result;
use broadcast_common::Parse;
use transmux::{
    AVCConfigurationBox, AVCDecoderConfigurationRecord, AudioSpecificConfig, CodecConfig,
    DecoderConfigDescriptor, DecoderSpecificInfo, ESDescriptor, EsdsBox, ObjectTypeIndication,
    SLConfigDescriptor, Sample, StreamType, decode_avc_sps,
};
use tracing::warn;

const CODEC_ID_AVC: u8 = 7;
const FRAME_TYPE_KEYFRAME: u8 = 1;
const SOUND_FORMAT_AAC: u8 = 10;

const AVC_PACKET_SEQUENCE_HEADER: u8 = 0;
const AVC_PACKET_NALU: u8 = 1;
const AAC_PACKET_SEQUENCE_HEADER: u8 = 0;
const AAC_PACKET_RAW: u8 = 1;

const OTI_MPEG4_AUDIO: u8 = 0x40;
const STREAM_TYPE_AUDIO: u8 = 0x05;
const ESDS_AUDIO_ES_ID: u16 = 1;
const SL_CONFIG_PREDEFINED_MP4: u8 = 0x02;
const AUDIO_SAMPLE_SIZE_BITS: u16 = 16;

/// Cap on a single NALU / AAC frame payload we will buffer (bytes).
const MAX_SAMPLE_BYTES: usize = 8 * 1024 * 1024;

/// Max RTMP DTS delta (ms) accepted as a normal sample duration.
///
/// Larger gaps (encoder stall, timestamp reset, or one-track discontinuity) used
/// to be **clamped** to the default frame duration. That under-counted one track
/// while the other kept accumulating real time — producing multi-minute A/V
/// `tfdt` skew (seen on ubn/ubnbus). Emit [`AccessUnit::TimelineDiscontinuity`]
/// instead so the packager rotates to a fresh CMAF generation.
///
/// Also treat true DTS rewinds (`dts < prev`) as discontinuities even when the
/// wrapping delta would otherwise look like a huge forward jump — encoder
/// restarts often reset one track a few hundred ms before the other.
const MAX_SAMPLE_GAP_MS: u32 = 5_000;

/// Single-track DTS jump above this (ms) while the other track has not jumped
/// with it is treated as a discontinuity — even when below [`MAX_SAMPLE_GAP_MS`].
/// Independent duration accumulation would otherwise bake a permanent A/V tfdt
/// offset into the CMAF generation.
const MAX_ASYM_JUMP_MS: u32 = 1_000;

/// Media timescale used for FLV/RTMP timestamps (milliseconds).
pub const TIMESCALE: u32 = 1000;

pub const VIDEO_TRACK_ID: u32 = 1;
pub const AUDIO_TRACK_ID: u32 = 2;

#[derive(Debug, Clone)]
pub enum AccessUnit {
    VideoConfig { config: CodecConfig },
    AudioConfig { config: CodecConfig },
    VideoSample(Sample),
    AudioSample(Sample),
    /// RTMP DTS jumped or rewound beyond [`MAX_SAMPLE_GAP_MS`] on one track.
    /// Packager must rotate so A/V `tfdt` timelines stay aligned.
    TimelineDiscontinuity {
        track: &'static str,
        gap_ms: u32,
    },
}

#[derive(Debug, Default)]
pub struct FlvDemux {
    pending_video: Option<(Sample, u32)>,
    pending_audio: Option<(Sample, u32)>,
    default_video_duration: u32,
    default_audio_duration: u32,
    /// First RTMP DTS observed on each track in the current generation.
    first_video_dts: Option<u32>,
    first_audio_dts: Option<u32>,
    /// `max(first_video, first_audio)` once both tracks have been seen.
    /// Samples below this are dropped so both CMAF tracks start at the same
    /// RTMP clock — Segmenter always begins `tfdt` at 0 per track, so a shared
    /// epoch is required to avoid a permanent A/V offset after publisher restart.
    sync_epoch_dts: Option<u32>,
    /// Last finalized sample boundary DTS per track (for asymmetric jump checks).
    last_video_dts: Option<u32>,
    last_audio_dts: Option<u32>,
}

impl FlvDemux {
    /// Create a demuxer with default per-frame duration estimates for video and audio.
    pub fn new() -> Self {
        Self {
            default_video_duration: 33,
            default_audio_duration: 23,
            ..Self::default()
        }
    }

    /// Clear A/V timeline state after a discontinuity so the next generation
    /// re-seals a shared RTMP DTS epoch.
    fn reset_timeline_clock(&mut self) {
        self.first_video_dts = None;
        self.first_audio_dts = None;
        self.sync_epoch_dts = None;
        self.last_video_dts = None;
        self.last_audio_dts = None;
    }

    fn note_track_dts(&mut self, is_video: bool, dts_ms: u32) {
        if is_video {
            if self.first_video_dts.is_none() {
                self.first_video_dts = Some(dts_ms);
            }
        } else if self.first_audio_dts.is_none() {
            self.first_audio_dts = Some(dts_ms);
        }
        if self.sync_epoch_dts.is_none() {
            if let (Some(v), Some(a)) = (self.first_video_dts, self.first_audio_dts) {
                let epoch = v.max(a);
                self.sync_epoch_dts = Some(epoch);
                tracing::info!(
                    sync_epoch_dts = epoch,
                    first_video_dts = v,
                    first_audio_dts = a,
                    "sealed shared RTMP DTS epoch for A/V tfdt alignment"
                );
            }
        }
    }

    /// Parse one FLV/RTMP video tag and return any completed access units.
    pub fn push_video(&mut self, data: &[u8], dts_ms: u32) -> Result<Vec<AccessUnit>> {
        if data.len() < 5 {
            return Ok(vec![]);
        }
        let frame_type = data[0] >> 4;
        let codec_id = data[0] & 0x0f;
        if codec_id != CODEC_ID_AVC {
            warn!(codec_id, "skipping non-AVC video tag");
            return Ok(vec![]);
        }

        let packet_type = data[1];
        let composition_time = read_si24(&data[2..5]);
        let payload = &data[5..];
        if payload.len() > MAX_SAMPLE_BYTES {
            warn!(len = payload.len(), "skipping oversized video payload");
            return Ok(vec![]);
        }

        match packet_type {
            AVC_PACKET_SEQUENCE_HEADER => {
                if payload.is_empty() {
                    return Ok(vec![]);
                }
                let record = match parse_avc_decoder_config(payload) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("avcC parse failed (skipped): {e}");
                        return Ok(vec![]);
                    }
                };
                let (width, height) = record
                    .sps
                    .first()
                    .and_then(|sps| decode_avc_sps(&sps.0).ok())
                    .map(|info| (info.width as u16, info.height as u16))
                    .unwrap_or((0, 0));
                let config = CodecConfig::Avc {
                    config: AVCConfigurationBox::new(record),
                    width,
                    height,
                };
                Ok(vec![AccessUnit::VideoConfig { config }])
            }
            AVC_PACKET_NALU => {
                let is_sync = frame_type == FRAME_TYPE_KEYFRAME;
                // FLV/RTMP AVC NALU payloads are already length-prefixed (AVCC).
                // Sample::from_annexb expects Annex-B start codes and would yield
                // empty data on normal RTMP ingest — producing m4s with 0-byte video.
                let sample = Sample::new(payload.to_vec(), 0, is_sync, composition_time);
                Ok(self.enqueue_video(sample, dts_ms))
            }
            _ => Ok(vec![]),
        }
    }

    /// Parse one FLV/RTMP audio tag and return any completed access units.
    pub fn push_audio(&mut self, data: &[u8], dts_ms: u32) -> Result<Vec<AccessUnit>> {
        if data.len() < 2 {
            return Ok(vec![]);
        }
        let sound_format = data[0] >> 4;
        if sound_format != SOUND_FORMAT_AAC {
            warn!(sound_format, "skipping non-AAC audio tag");
            return Ok(vec![]);
        }

        let packet_type = data[1];
        let payload = &data[2..];
        if payload.len() > MAX_SAMPLE_BYTES {
            warn!(len = payload.len(), "skipping oversized audio payload");
            return Ok(vec![]);
        }

        match packet_type {
            AAC_PACKET_SEQUENCE_HEADER => {
                if payload.is_empty() {
                    return Ok(vec![]);
                }
                let asc = match AudioSpecificConfig::parse(payload) {
                    Ok(a) => a,
                    Err(e) => {
                        warn!("AAC ASC parse failed (skipped): {e}");
                        return Ok(vec![]);
                    }
                };
                let sample_rate = asc_rate_hz(&asc);
                let channels = asc.channel_configuration.raw() as u16;
                if sample_rate > 0 {
                    self.default_audio_duration =
                        ((1024u64 * 1000) / u64::from(sample_rate)).max(1) as u32;
                }
                let esds = build_aac_esds(payload.to_vec());
                let config = CodecConfig::Aac {
                    esds,
                    channel_count: channels.max(1),
                    sample_rate: sample_rate.max(1),
                    sample_size: AUDIO_SAMPLE_SIZE_BITS,
                };
                Ok(vec![AccessUnit::AudioConfig { config }])
            }
            AAC_PACKET_RAW => {
                let sample = Sample::from_raw(payload.to_vec(), 0);
                Ok(self.enqueue_audio(sample, dts_ms))
            }
            _ => Ok(vec![]),
        }
    }

    /// Emit any buffered samples that have no following frame to compute duration from.
    pub fn flush(&mut self) -> Vec<AccessUnit> {
        let mut out = Vec::new();
        if let Some((mut sample, _)) = self.pending_video.take() {
            if sample.duration == 0 {
                sample.duration = self.default_video_duration.max(1);
            }
            out.push(AccessUnit::VideoSample(sample));
        }
        if let Some((mut sample, _)) = self.pending_audio.take() {
            if sample.duration == 0 {
                sample.duration = self.default_audio_duration.max(1);
            }
            out.push(AccessUnit::AudioSample(sample));
        }
        out
    }

    /// Buffer a video sample and finalize the previous one's duration from DTS delta.
    fn enqueue_video(&mut self, sample: Sample, dts_ms: u32) -> Vec<AccessUnit> {
        self.note_track_dts(true, dts_ms);
        self.enqueue_sample(true, sample, dts_ms)
    }

    /// Buffer an audio sample and finalize the previous one's duration from DTS delta.
    fn enqueue_audio(&mut self, sample: Sample, dts_ms: u32) -> Vec<AccessUnit> {
        self.note_track_dts(false, dts_ms);
        self.enqueue_sample(false, sample, dts_ms)
    }

    fn enqueue_sample(&mut self, is_video: bool, sample: Sample, dts_ms: u32) -> Vec<AccessUnit> {
        let mut out = Vec::new();
        let default = if is_video {
            self.default_video_duration
        } else {
            self.default_audio_duration
        };
        let other_last = if is_video {
            self.last_audio_dts
        } else {
            self.last_video_dts
        };
        let sync_epoch = self.sync_epoch_dts;

        let prev_pair = if is_video {
            self.pending_video.take()
        } else {
            self.pending_audio.take()
        };
        let Some((mut prev, prev_dts)) = prev_pair else {
            if is_video {
                self.pending_video = Some((sample, dts_ms));
            } else {
                self.pending_audio = Some((sample, dts_ms));
            }
            return out;
        };

        match sample_duration_ms(dts_ms, prev_dts, default, other_last) {
            Err(gap_ms) => {
                let track = if is_video { "video" } else { "audio" };
                warn!(
                    gap_ms,
                    prev_dts, dts_ms, track, "RTMP DTS discontinuity; requesting timeline rotate"
                );
                out.push(AccessUnit::TimelineDiscontinuity { track, gap_ms });
                self.reset_timeline_clock();
                // Drop the other track's pending — it belongs to the dead generation.
                self.pending_video = None;
                self.pending_audio = None;
                self.note_track_dts(is_video, dts_ms);
                if is_video {
                    self.pending_video = Some((sample, dts_ms));
                } else {
                    self.pending_audio = Some((sample, dts_ms));
                }
            }
            Ok(dur) => {
                // Wait until both A/V have been seen so we can share one RTMP epoch.
                let Some(epoch) = sync_epoch else {
                    if is_video {
                        self.pending_video = Some((sample, dts_ms));
                    } else {
                        self.pending_audio = Some((sample, dts_ms));
                    }
                    return out;
                };
                // Drop pre-epoch samples so both tracks' tfdt=0 map to the same DTS.
                if prev_dts < epoch {
                    if is_video {
                        self.pending_video = Some((sample, dts_ms));
                    } else {
                        self.pending_audio = Some((sample, dts_ms));
                    }
                    return out;
                }

                prev.duration = dur;
                if is_video {
                    self.default_video_duration = dur;
                    self.last_video_dts = Some(dts_ms);
                    out.push(AccessUnit::VideoSample(prev));
                    self.pending_video = Some((sample, dts_ms));
                } else {
                    self.default_audio_duration = dur;
                    self.last_audio_dts = Some(dts_ms);
                    out.push(AccessUnit::AudioSample(prev));
                    self.pending_audio = Some((sample, dts_ms));
                }
            }
        }
        out
    }
}

/// Compute sample duration from consecutive RTMP DTS values.
///
/// Returns `Err(gap_ms)` when the delta is a discontinuity (too large, a
/// rewind, or an asymmetric single-track jump), instead of silently clamping.
fn sample_duration_ms(
    dts_ms: u32,
    prev_dts: u32,
    default: u32,
    other_last_dts: Option<u32>,
) -> Result<u32, u32> {
    if dts_ms < prev_dts {
        // Encoder / publisher restart often rewinds one track first.
        return Err(prev_dts - dts_ms);
    }
    let dur = dts_ms - prev_dts;
    if dur == 0 {
        return Ok(default.max(1));
    }
    if dur > MAX_SAMPLE_GAP_MS {
        return Err(dur);
    }
    // One track leaped forward while the other is still near `prev_dts`.
    if dur > MAX_ASYM_JUMP_MS {
        if let Some(other) = other_last_dts {
            let ahead_of_other = dts_ms.saturating_sub(other);
            if ahead_of_other > MAX_ASYM_JUMP_MS {
                return Err(dur);
            }
        }
    }
    Ok(dur)
}

/// ISO high-profile ids that require the avcC chroma/bit-depth trailer.
fn is_high_avc_profile(profile: u8) -> bool {
    matches!(profile, 100 | 110 | 122 | 244)
}

/// Default High-profile avcC trailer: chroma=4:2:0, 8-bit, no SPSExt.
/// Many soft-x264 RTMP publishers omit these bytes even though profile is High.
const DEFAULT_HIGH_PROFILE_AVCC_EXT: [u8; 4] = [0xFC | 0x01, 0xF8, 0xF8, 0x00];

/// Parse `avcC`, tolerating High-profile records that omit the ISO extension.
///
/// Strict `transmux` parsing fails with e.g.
/// `buffer too short … (while parsing chroma_format byte)` when x264 soft
/// encode ships High (100) without the 4-byte chroma/bit-depth trailer.
/// NVENC publishers usually include a complete/correct trailer, so they work.
fn parse_avc_decoder_config(payload: &[u8]) -> std::result::Result<AVCDecoderConfigurationRecord, transmux::Error> {
    match AVCDecoderConfigurationRecord::parse(payload) {
        Ok(r) => Ok(r),
        Err(first_err) => {
            if payload.len() < 2 || !is_high_avc_profile(payload[1]) {
                return Err(first_err);
            }
            let msg = first_err.to_string();
            let truncated_ext = msg.contains("chroma_format")
                || msg.contains("bit_depth_luma")
                || msg.contains("bit_depth_chroma")
                || msg.contains("numOfSequenceParameterSetExt");
            if !truncated_ext {
                return Err(first_err);
            }
            let mut padded = payload.to_vec();
            padded.extend_from_slice(&DEFAULT_HIGH_PROFILE_AVCC_EXT);
            match AVCDecoderConfigurationRecord::parse(&padded) {
                Ok(r) => {
                    warn!(
                        profile = payload[1],
                        orig_len = payload.len(),
                        "avcC missing high-profile extension; padded 4:2:0/8-bit defaults (x264 soft RTMP)"
                    );
                    Ok(r)
                }
                Err(_) => Err(first_err),
            }
        }
    }
}

/// Read a signed 24-bit big-endian integer (composition time offset).
fn read_si24(b: &[u8]) -> i32 {
    let raw = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
    if raw & 0x0080_0000 != 0 {
        (raw | 0xFF00_0000) as i32
    } else {
        raw as i32
    }
}

/// Resolve AAC sample rate in Hz from an AudioSpecificConfig.
fn asc_rate_hz(asc: &AudioSpecificConfig) -> u32 {
    if let Some(f) = asc.sampling_frequency {
        return f;
    }
    match asc.sampling_frequency_index.raw() {
        0 => 96000,
        1 => 88200,
        2 => 64000,
        3 => 48000,
        4 => 44100,
        5 => 32000,
        6 => 24000,
        7 => 22050,
        8 => 16000,
        9 => 12000,
        10 => 11025,
        11 => 8000,
        12 => 7350,
        _ => 0,
    }
}

/// Build an MPEG-4 `esds` box wrapping the raw AAC AudioSpecificConfig bytes.
fn build_aac_esds(asc_bytes: Vec<u8>) -> EsdsBox {
    EsdsBox::new(ESDescriptor {
        es_id: ESDS_AUDIO_ES_ID,
        stream_dependence_flag: false,
        url_flag: false,
        ocr_stream_flag: false,
        stream_priority: 0,
        depends_on_es_id: None,
        url: None,
        ocr_es_id: None,
        decoder_config: Some(DecoderConfigDescriptor {
            object_type_indication: ObjectTypeIndication(OTI_MPEG4_AUDIO),
            stream_type: StreamType(STREAM_TYPE_AUDIO),
            up_stream: false,
            buffer_size_db: 0,
            max_bitrate: 0,
            avg_bitrate: 0,
            decoder_specific_info: Some(DecoderSpecificInfo { data: asc_bytes }),
        }),
        sl_config: Some(SLConfigDescriptor {
            body: vec![SL_CONFIG_PREDEFINED_MP4],
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avc_nalu_keeps_length_prefixed_payload() {
        // RTMP AVC NALU body is AVCC (4-byte length + NAL), not Annex-B.
        let nal = [0x65u8, 0x88, 0x84];
        let mut payload = Vec::new();
        payload.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        payload.extend_from_slice(&nal);
        let mut tag = vec![0x17, 0x01, 0x00, 0x00, 0x00]; // keyframe + AVC NALU
        tag.extend_from_slice(&payload);

        let mut demux = FlvDemux::new();
        // Prime audio so the shared RTMP epoch seals (both tracks required).
        let _ = demux.push_audio(&[0xAF, 0x01, 0x11], 0).unwrap();
        let _ = demux.push_video(&tag, 0).unwrap();
        // Second frame finalizes the first sample's duration.
        let mut tag2 = vec![0x27, 0x01, 0x00, 0x00, 0x00];
        tag2.extend_from_slice(&payload);
        let aus = demux.push_video(&tag2, 33).unwrap();
        let AccessUnit::VideoSample(sample) = &aus[0] else {
            panic!("expected video sample, got {aus:?}");
        };
        assert_eq!(sample.data, payload);
        assert!(!sample.data.is_empty());
        assert!(sample.is_sync);
    }

    #[test]
    fn high_profile_avcc_without_extension_is_tolerated() {
        // Mimic soft-x264 RTMP: High (100) + SPS/PPS, no chroma/bit-depth trailer.
        let mut body = vec![
            0x01,        // configurationVersion
            0x64,        // High profile
            0x00,        // compatibility
            0x1F,        // level 3.1
            0xFC | 0x03, // lengthSizeMinusOne=3
            0xE0 | 0x01, // numSPS=1
        ];
        let sps = vec![0x67, 0x64, 0x00, 0x1F, 0xAC, 0xD9];
        body.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        body.extend_from_slice(&sps);
        body.push(0x01); // numPPS
        let pps = vec![0x68, 0xEB, 0xE3, 0xCB];
        body.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        body.extend_from_slice(&pps);

        assert!(
            AVCDecoderConfigurationRecord::parse(&body).is_err(),
            "strict parse must fail without high-profile ext"
        );
        let record = parse_avc_decoder_config(&body).expect("tolerant parse");
        assert_eq!(record.profile_indication, 100);
        assert_eq!(record.chroma_format, Some(1));
        assert_eq!(record.bit_depth_luma_minus8, Some(0));
        assert_eq!(record.sps.len(), 1);
        assert_eq!(record.pps.len(), 1);

        // FLV sequence header path should emit VideoConfig.
        let mut tag = vec![0x17, 0x00, 0x00, 0x00, 0x00];
        tag.extend_from_slice(&body);
        let mut demux = FlvDemux::new();
        let aus = demux.push_video(&tag, 0).unwrap();
        assert!(matches!(aus[0], AccessUnit::VideoConfig { .. }));
    }

    #[test]
    fn large_dts_gap_emits_timeline_discontinuity_not_clamped_duration() {
        let nal = [0x65u8, 0x88, 0x84];
        let mut payload = Vec::new();
        payload.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        payload.extend_from_slice(&nal);
        let mut tag = vec![0x17, 0x01, 0x00, 0x00, 0x00];
        tag.extend_from_slice(&payload);

        let mut demux = FlvDemux::new();
        let _ = demux.push_video(&tag, 0).unwrap();
        // Jump >> MAX_SAMPLE_GAP_MS — old code clamped to ~33ms and poisoned tfdt.
        let mut tag2 = vec![0x27, 0x01, 0x00, 0x00, 0x00];
        tag2.extend_from_slice(&payload);
        let aus = demux.push_video(&tag2, 10_000).unwrap();
        assert!(
            matches!(
                aus.as_slice(),
                [AccessUnit::TimelineDiscontinuity {
                    track: "video",
                    gap_ms: 10_000
                }]
            ),
            "got {aus:?}"
        );
    }

    #[test]
    fn sample_duration_rejects_gaps_over_five_seconds() {
        assert_eq!(sample_duration_ms(33, 0, 33, None), Ok(33));
        assert_eq!(sample_duration_ms(100, 100, 33, None), Ok(33)); // zero delta → default
        assert!(sample_duration_ms(6_000, 0, 33, None).is_err());
    }

    #[test]
    fn sample_duration_rejects_dts_rewind() {
        assert_eq!(sample_duration_ms(500, 1_000, 33, None), Err(500));
    }

    #[test]
    fn sample_duration_rejects_asymmetric_jump_under_five_seconds() {
        // 2s jump on this track while the other is still near the previous DTS.
        assert_eq!(
            sample_duration_ms(3_000, 1_000, 33, Some(1_020)),
            Err(2_000)
        );
        // Both tracks jumped together — accept as a long (but <5s) frame.
        assert_eq!(
            sample_duration_ms(3_000, 1_000, 33, Some(2_900)),
            Ok(2_000)
        );
    }

    #[test]
    fn shared_sync_epoch_drops_early_audio_so_av_start_aligned() {
        let nal = [0x65u8, 0x88, 0x84];
        let mut vpayload = Vec::new();
        vpayload.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        vpayload.extend_from_slice(&nal);
        let vtag = |dts: u32, key: bool| {
            let mut tag = vec![if key { 0x17 } else { 0x27 }, 0x01, 0x00, 0x00, 0x00];
            tag.extend_from_slice(&vpayload);
            (tag, dts)
        };
        let atag = |dts: u32| (vec![0xAFu8, 0x01, 0x11, 0x22], dts);

        let mut demux = FlvDemux::new();
        // Video starts at 0; audio at 500 → epoch = 500.
        let (t, d) = vtag(0, true);
        let _ = demux.push_video(&t, d).unwrap();
        let (t, d) = vtag(33, false);
        assert!(
            demux.push_video(&t, d).unwrap().is_empty(),
            "hold video until audio seals epoch"
        );
        let (t, d) = atag(500);
        let _ = demux.push_audio(&t, d).unwrap();
        assert_eq!(demux.sync_epoch_dts, Some(500));

        // Pre-epoch video must not emit; post-epoch pair should.
        let (t, d) = vtag(500, true);
        let _ = demux.push_video(&t, d).unwrap();
        let (t, d) = vtag(533, false);
        let aus = demux.push_video(&t, d).unwrap();
        assert!(
            aus.iter().any(|a| matches!(a, AccessUnit::VideoSample(_))),
            "got {aus:?}"
        );

        let (t, d) = atag(523);
        let _ = demux.push_audio(&t, d).unwrap();
        let (t, d) = atag(546);
        let aus = demux.push_audio(&t, d).unwrap();
        assert!(
            aus.iter().any(|a| matches!(a, AccessUnit::AudioSample(_))),
            "got {aus:?}"
        );
    }
}

