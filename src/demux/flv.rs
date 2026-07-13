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
}

#[derive(Debug, Default)]
pub struct FlvDemux {
    pending_video: Option<(Sample, u32)>,
    pending_audio: Option<(Sample, u32)>,
    default_video_duration: u32,
    default_audio_duration: u32,
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
                let record = match AVCDecoderConfigurationRecord::parse(payload) {
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
        let mut out = Vec::new();
        if let Some((mut prev, prev_dts)) = self.pending_video.take() {
            let dur = dts_ms.wrapping_sub(prev_dts);
            let dur = if dur == 0 || dur > 5_000 {
                self.default_video_duration.max(1)
            } else {
                dur
            };
            prev.duration = dur;
            self.default_video_duration = dur;
            out.push(AccessUnit::VideoSample(prev));
        }
        self.pending_video = Some((sample, dts_ms));
        out
    }

    /// Buffer an audio sample and finalize the previous one's duration from DTS delta.
    fn enqueue_audio(&mut self, sample: Sample, dts_ms: u32) -> Vec<AccessUnit> {
        let mut out = Vec::new();
        if let Some((mut prev, prev_dts)) = self.pending_audio.take() {
            let dur = dts_ms.wrapping_sub(prev_dts);
            let dur = if dur == 0 || dur > 5_000 {
                self.default_audio_duration.max(1)
            } else {
                dur
            };
            prev.duration = dur;
            self.default_audio_duration = dur;
            out.push(AccessUnit::AudioSample(prev));
        }
        self.pending_audio = Some((sample, dts_ms));
        out
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
