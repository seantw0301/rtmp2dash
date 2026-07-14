use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use transmux::CodecConfig;

#[derive(Debug, Clone)]
pub struct MpdTrackInfo {
    pub codecs: String,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub sample_rate: Option<u32>,
}

/// One on-disk media segment as advertised in `SegmentTimeline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineEntry {
    pub number: u64,
    /// Absolute media start time (tfdt baseMediaDecodeTime) in MPD timescale ticks.
    ///
    /// Must NOT reset when the sliding window advances — players track their
    /// playback position in Period media time, so the timeline has to keep
    /// moving forward across MPD updates.
    pub start_ticks: u64,
    /// Media duration in MPD timescale ticks (1000 = milliseconds).
    pub duration_ticks: u64,
}

impl MpdTrackInfo {
    /// Build MPD representation metadata (codecs, resolution) from a video codec config.
    pub fn from_video(config: &CodecConfig) -> Self {
        match config {
            CodecConfig::Avc {
                config,
                width,
                height,
                ..
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

/// Build a live MPD whose timeline matches **actual** segment media durations.
///
/// Uses `SegmentTimeline` (no fixed `@duration`) so players request only numbers
/// present in `entries`. `availabilityStartTime` is anchored so the window ends
/// at ≈ `now`.
pub fn render_live_mpd(
    entries: &[TimelineEntry],
    availability_start_time: DateTime<Utc>,
    video: &MpdTrackInfo,
    audio: Option<&MpdTrackInfo>,
) -> String {
    assert!(!entries.is_empty(), "entries must not be empty");
    let start_number = entries[0].number;
    let max_ticks = entries
        .iter()
        .map(|e| e.duration_ticks.max(1))
        .max()
        .unwrap_or(2000);
    let buffer_depth_secs = entries
        .iter()
        .map(|e| e.duration_ticks as f64 / 1000.0)
        .sum::<f64>()
        .max(1.0);
    let avg_secs = buffer_depth_secs / entries.len() as f64;
    let min_update = avg_secs.clamp(1.0, 4.0);
    let suggested = (avg_secs * 3.0)
        .max(4.0)
        .min(buffer_depth_secs * 0.5)
        .max(1.0);
    let publish_time = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let availability_start_time =
        availability_start_time.to_rfc3339_opts(SecondsFormat::Secs, true);

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

    let timeline = render_segment_timeline(entries);

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
                         initialization="init.mp4"
                         media="seg_$Number$.m4s"
                         startNumber="{start_number}">
{timeline}        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>
"#,
        buffer_depth = buffer_depth_secs,
        min_buffer = max_ticks as f64 / 1000.0,
    )
}

/// Compact `SegmentTimeline` with run-length `r` when consecutive equal durations.
///
/// Uses each entry's absolute `start_ticks` for `t`, so the timeline keeps
/// advancing as the sliding window moves (never resets to 0 between MPD updates).
fn render_segment_timeline(entries: &[TimelineEntry]) -> String {
    let mut out = String::from("          <SegmentTimeline>\n");
    let mut i = 0usize;
    let mut expected_t: Option<u64> = None;
    while i < entries.len() {
        let d = entries[i].duration_ticks.max(1);
        let t = entries[i].start_ticks;
        let mut run = 0u64;
        while i + 1 + (run as usize) < entries.len() {
            let next = &entries[i + 1 + (run as usize)];
            let contiguous = next.duration_ticks.max(1) == d
                && next.number == entries[i].number + 1 + run
                && next.start_ticks == t.saturating_add(d.saturating_mul(run + 1));
            if !contiguous {
                break;
            }
            run += 1;
        }
        // Emit `t` on the first S and whenever there is a gap/drift in media time.
        let need_t = expected_t != Some(t);
        match (need_t, run > 0) {
            (true, true) => out.push_str(&format!(
                "            <S t=\"{t}\" d=\"{d}\" r=\"{run}\"/>\n"
            )),
            (true, false) => out.push_str(&format!("            <S t=\"{t}\" d=\"{d}\"/>\n")),
            (false, true) => out.push_str(&format!("            <S d=\"{d}\" r=\"{run}\"/>\n")),
            (false, false) => out.push_str(&format!("            <S d=\"{d}\"/>\n")),
        }
        let count = run + 1;
        expected_t = Some(t.saturating_add(d.saturating_mul(count)));
        i += count as usize;
    }
    out.push_str("          </SegmentTimeline>\n");
    out
}

/// Anchor AST so the **live edge** (end of the newest segment in media time)
/// maps to `now`. Called once per generation; the result must then stay fixed
/// for every subsequent MPD update, otherwise players lose their position.
pub fn availability_start_for_live_edge(
    now: DateTime<Utc>,
    entries: &[TimelineEntry],
) -> DateTime<Utc> {
    let edge_ms = entries
        .last()
        .map(|e| e.start_ticks.saturating_add(e.duration_ticks.max(1)) as i64)
        .unwrap_or(0);
    now - ChronoDuration::milliseconds(edge_ms.max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracks() -> (MpdTrackInfo, MpdTrackInfo) {
        (
            MpdTrackInfo {
                codecs: "avc1.42E01E".into(),
                width: Some(640),
                height: Some(360),
                sample_rate: None,
            },
            MpdTrackInfo {
                codecs: "mp4a.40.2".into(),
                width: None,
                height: None,
                sample_rate: Some(44100),
            },
        )
    }

    #[test]
    fn timeline_uses_actual_durations_not_fixed_two_seconds() {
        let (video, audio) = tracks();
        let entries = [
            TimelineEntry {
                number: 10,
                start_ticks: 18_000,
                duration_ticks: 2100,
            },
            TimelineEntry {
                number: 11,
                start_ticks: 20_100,
                duration_ticks: 1900,
            },
            TimelineEntry {
                number: 12,
                start_ticks: 22_000,
                duration_ticks: 2400,
            },
        ];
        let ast = DateTime::parse_from_rfc3339("2026-07-13T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let xml = render_live_mpd(&entries, ast, &video, Some(&audio));
        assert!(xml.contains("startNumber=\"10\""));
        assert!(xml.contains("<SegmentTimeline>"));
        assert!(xml.contains("t=\"18000\""));
        assert!(xml.contains("d=\"2100\""));
        assert!(xml.contains("d=\"1900\""));
        assert!(xml.contains("d=\"2400\""));
        assert!(!xml.contains("duration=\"2000\""));
        assert!(xml.contains("timeShiftBufferDepth=\"PT6.400S\""));
    }

    #[test]
    fn timeline_runs_collapse_equal_durations() {
        let entries = [
            TimelineEntry {
                number: 1,
                start_ticks: 0,
                duration_ticks: 2000,
            },
            TimelineEntry {
                number: 2,
                start_ticks: 2000,
                duration_ticks: 2000,
            },
            TimelineEntry {
                number: 3,
                start_ticks: 4000,
                duration_ticks: 2000,
            },
        ];
        let xml = render_segment_timeline(&entries);
        assert!(xml.contains("r=\"2\""));
        assert!(xml.contains("t=\"0\""));
    }

    #[test]
    fn timeline_preserves_absolute_start_after_window_slide() {
        // Window slid: first advertised segment starts at media time 60s.
        let entries = [
            TimelineEntry {
                number: 31,
                start_ticks: 60_000,
                duration_ticks: 2000,
            },
            TimelineEntry {
                number: 32,
                start_ticks: 62_000,
                duration_ticks: 2000,
            },
        ];
        let xml = render_segment_timeline(&entries);
        assert!(xml.contains("t=\"60000\""));
        assert!(!xml.contains("t=\"0\""));
        assert!(xml.contains("r=\"1\""));
    }

    #[test]
    fn timeline_emits_new_t_on_media_gap() {
        let entries = [
            TimelineEntry {
                number: 1,
                start_ticks: 0,
                duration_ticks: 2000,
            },
            // Gap: segment 2 starts at 5000 instead of 2000.
            TimelineEntry {
                number: 2,
                start_ticks: 5000,
                duration_ticks: 2000,
            },
        ];
        let xml = render_segment_timeline(&entries);
        assert!(xml.contains("t=\"0\""));
        assert!(xml.contains("t=\"5000\""));
    }

    #[test]
    fn availability_anchors_live_edge_to_now() {
        let now = DateTime::parse_from_rfc3339("2026-07-13T22:10:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let entries = [
            TimelineEntry {
                number: 1,
                start_ticks: 0,
                duration_ticks: 2100,
            },
            TimelineEntry {
                number: 2,
                start_ticks: 2100,
                duration_ticks: 1900,
            },
        ];
        let ast = availability_start_for_live_edge(now, &entries);
        assert_eq!(
            ast.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-07-13T22:09:56Z"
        );
    }
}
