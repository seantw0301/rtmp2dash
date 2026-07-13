use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use transmux::CodecConfig;

#[derive(Debug, Clone)]
pub struct MpdTrackInfo {
    pub codecs: String,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub sample_rate: Option<u32>,
}

impl MpdTrackInfo {
    /// Build MPD representation metadata (codecs, resolution) from a video codec config.
    pub fn from_video(config: &CodecConfig) -> Self {
        match config {
            CodecConfig::Avc {
                config, width, height, ..
            } => Self {
                codecs: format!(
                    "avc1.{:02X}{:02X}{:02X}",
                    config.config.profile_indication,
                    config.config.profile_compatibility,
                    config.config.level_indication
                ),
                width: Some(*width),
                height: Some(*height),
                sample_rate: None,
            },
            _ => Self {
                codecs: "avc1.42E01E".to_string(),
                width: None,
                height: None,
                sample_rate: None,
            },
        }
    }

    /// Build MPD representation metadata (codecs, sample rate) from an audio codec config.
    pub fn from_audio(config: &CodecConfig) -> Self {
        match config {
            CodecConfig::Aac { sample_rate, .. } => Self {
                codecs: "mp4a.40.2".to_string(),
                width: None,
                height: None,
                sample_rate: Some(*sample_rate),
            },
            _ => Self {
                codecs: "mp4a.40.2".to_string(),
                width: None,
                height: None,
                sample_rate: Some(44100),
            },
        }
    }
}

/// Build a live (dynamic) MPD for multiplexed CMAF segments.
///
/// `availabilityStartTime` is recomputed on every refresh so that wall-clock based
/// players (VLC) map the live edge onto the current sliding window (`startNumber`…
/// `latest_number`), instead of drifting ahead after the stream has been up a while.
pub fn render_live_mpd(
    segment_duration_secs: f64,
    window_segments: usize,
    start_number: u64,
    latest_number: u64,
    video: &MpdTrackInfo,
    audio: Option<&MpdTrackInfo>,
) -> String {
    let duration_ticks = (segment_duration_secs * 1000.0).round() as u64;
    let buffer_depth = segment_duration_secs * window_segments as f64;
    let min_update = segment_duration_secs.max(1.0);
    let suggested = (segment_duration_secs * 2.0).max(2.0);
    let publish_time = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);

    // Segments present in the sliding window (inclusive).
    let segs_available = if latest_number >= start_number {
        latest_number - start_number + 1
    } else {
        0
    };
    // Place AST so: startNumber ≈ available at AST, and live edge ≈ latest.
    // availability(N) = AST + (N - startNumber) * duration
    // ⇒ AST = now - (latest - startNumber) * duration - suggestedDelay
    let elapsed_ms = if segs_available > 0 {
        let from_start_to_latest =
            (segs_available.saturating_sub(1) as f64) * segment_duration_secs * 1000.0;
        (from_start_to_latest + suggested * 1000.0) as i64
    } else {
        (buffer_depth * 1000.0) as i64
    };
    let availability_start_time = (Utc::now() - ChronoDuration::milliseconds(elapsed_ms.max(0)))
        .to_rfc3339_opts(SecondsFormat::Secs, true);

    let codecs = match audio {
        Some(a) => format!("{},{}", video.codecs, a.codecs),
        None => video.codecs.clone(),
    };

    let width_attr = video
        .width
        .map(|w| format!(r#" width="{w}""#))
        .unwrap_or_default();
    let height_attr = video
        .height
        .map(|h| format!(r#" height="{h}""#))
        .unwrap_or_default();

    let audio_sampling = audio
        .and_then(|a| a.sample_rate)
        .map(|r| format!(r#" audioSamplingRate="{r}""#))
        .unwrap_or_default();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
     xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
     xsi:schemaLocation="urn:mpeg:dash:schema:mpd:2011 DASH-MPD.xsd"
     profiles="urn:mpeg:dash:profile:isoff-live:2011"
     type="dynamic"
     availabilityStartTime="{availability_start_time}"
     publishTime="{publish_time}"
     minimumUpdatePeriod="PT{min_update:.3}S"
     timeShiftBufferDepth="PT{buffer_depth:.3}S"
     suggestedPresentationDelay="PT{suggested:.3}S"
     minBufferTime="PT{min_buffer:.3}S">
  <UTCTiming schemeIdUri="urn:mpeg:dash:utc:direct:2014" value="{publish_time}"/>
  <Period id="0" start="PT0S">
    <AdaptationSet id="0" contentType="video" mimeType="video/mp4" segmentAlignment="true" startWithSAP="1" bitstreamSwitching="true"{width_attr}{height_attr}{audio_sampling}>
      <Representation id="0" bandwidth="2500000" codecs="{codecs}"{width_attr}{height_attr}>
        <SegmentTemplate timescale="1000"
                         duration="{duration_ticks}"
                         initialization="init.mp4"
                         media="seg_$Number$.m4s"
                         startNumber="{start_number}"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>
"#,
        min_buffer = segment_duration_secs,
    )
}


