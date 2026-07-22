//! Detect and report A/V `tfdt` (baseMediaDecodeTime) skew in CMAF fragments.
//!
//! Independent per-track duration accumulation (from RTMP DTS deltas) can drift
//! when one track clamps a large timestamp jump while the other does not. Android
//! / Rockchip MPEG-TS remux of skewed fragments presents as "packet order chaos".

use crate::demux::{AUDIO_TRACK_ID, VIDEO_TRACK_ID};
use std::sync::atomic::{AtomicU64, Ordering};

/// Audio minus video `tfdt` in milliseconds (timescale 1000).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvTfdtBases {
    pub video_ms: u64,
    pub audio_ms: u64,
}

impl AvTfdtBases {
    /// Signed skew: positive means audio timeline is ahead of video.
    pub fn skew_ms(self) -> i64 {
        self.audio_ms as i64 - self.video_ms as i64
    }
}

static CORRECTIONS_TOTAL: AtomicU64 = AtomicU64::new(0);
static LAST_SKEW_MS: AtomicU64 = AtomicU64::new(0);

/// Record one auto-correct (rotate / discontinuity) event for `/metrics`.
pub fn record_correction() {
    CORRECTIONS_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Total A/V skew corrections since process start.
pub fn corrections_total() -> u64 {
    CORRECTIONS_TOTAL.load(Ordering::Relaxed)
}

/// Last observed |skew| sample (ms), for metrics gauges.
pub fn last_abs_skew_ms() -> u64 {
    LAST_SKEW_MS.load(Ordering::Relaxed)
}

fn store_last_skew(skew_ms: i64) {
    LAST_SKEW_MS.store(skew_ms.unsigned_abs(), Ordering::Relaxed);
}

/// True when absolute A/V `tfdt` delta exceeds the configured tolerance.
pub fn exceeds_tolerance(skew_ms: i64, max_abs_ms: u64) -> bool {
    skew_ms.unsigned_abs() > max_abs_ms
}

/// Parse video + audio `tfdt` baseMediaDecodeTime from a multiplexed CMAF `m4s`.
pub fn parse_av_tfdt_ms(data: &[u8]) -> Option<AvTfdtBases> {
    let moof = find_box(data, b"moof")?;
    let mut video = None;
    let mut audio = None;
    let mut pos = 0usize;
    while pos < moof.len() {
        let (hdr, payload) = read_box_at(moof, pos)?;
        if &hdr.typ == b"traf" {
            if let Some((track_id, base)) = parse_traf_tfdt(payload) {
                match track_id {
                    VIDEO_TRACK_ID => video = Some(base),
                    AUDIO_TRACK_ID => audio = Some(base),
                    _ => {}
                }
            }
        }
        pos = hdr.end;
    }
    let bases = AvTfdtBases {
        video_ms: video?,
        audio_ms: audio?,
    };
    store_last_skew(bases.skew_ms());
    Some(bases)
}

fn parse_traf_tfdt(traf: &[u8]) -> Option<(u32, u64)> {
    let mut track_id = None;
    let mut base = None;
    let mut pos = 0usize;
    while pos < traf.len() {
        let (hdr, payload) = read_box_at(traf, pos)?;
        match &hdr.typ {
            b"tfhd" => {
                if payload.len() >= 8 {
                    track_id = Some(u32::from_be_bytes(payload[4..8].try_into().ok()?));
                }
            }
            b"tfdt" => {
                if payload.len() >= 8 {
                    let version = payload[0];
                    base = Some(if version == 1 {
                        if payload.len() < 12 {
                            return None;
                        }
                        u64::from_be_bytes(payload[4..12].try_into().ok()?)
                    } else {
                        u32::from_be_bytes(payload[4..8].try_into().ok()?) as u64
                    });
                }
            }
            _ => {}
        }
        pos = hdr.end;
    }
    Some((track_id?, base?))
}

struct BoxHeader {
    typ: [u8; 4],
    end: usize,
}

fn find_box<'a>(data: &'a [u8], want: &[u8; 4]) -> Option<&'a [u8]> {
    let mut pos = 0usize;
    while pos < data.len() {
        let (hdr, payload) = read_box_at(data, pos)?;
        if &hdr.typ == want {
            return Some(payload);
        }
        pos = hdr.end;
    }
    None
}

fn read_box_at(data: &[u8], pos: usize) -> Option<(BoxHeader, &[u8])> {
    if data.len() < pos + 8 {
        return None;
    }
    let size32 = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
    let mut typ = [0u8; 4];
    typ.copy_from_slice(&data[pos + 4..pos + 8]);
    let (header_len, total) = if size32 == 1 {
        if data.len() < pos + 16 {
            return None;
        }
        let large = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().ok()?) as usize;
        (16usize, large)
    } else if size32 == 0 {
        (8usize, data.len().saturating_sub(pos))
    } else {
        (8usize, size32)
    };
    if total < header_len || pos.checked_add(total)? > data.len() {
        return None;
    }
    let end = pos + total;
    let payload = &data[pos + header_len..end];
    Some((BoxHeader { typ, end }, payload))
}

/// Prometheus lines for A/V skew monitoring.
pub fn render_metrics(prefix: &str) -> String {
    format!(
        "{prefix}_av_tfdt_skew_corrections_total {}\n\
         {prefix}_av_tfdt_last_abs_skew_milliseconds {}\n",
        corrections_total(),
        last_abs_skew_ms()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tfdt_box(version: u8, base_time: u64) -> Vec<u8> {
        let mut b = Vec::new();
        let start = b.len();
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(b"tfdt");
        b.push(version);
        b.extend_from_slice(&[0, 0, 0]);
        if version == 1 {
            b.extend_from_slice(&base_time.to_be_bytes());
        } else {
            b.extend_from_slice(&(base_time as u32).to_be_bytes());
        }
        let size = (b.len() - start) as u32;
        b[start..start + 4].copy_from_slice(&size.to_be_bytes());
        b
    }

    fn tfhd_box(track_id: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&16u32.to_be_bytes());
        b.extend_from_slice(b"tfhd");
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&track_id.to_be_bytes());
        b
    }

    fn wrap_box(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
        b.extend_from_slice(typ);
        b.extend_from_slice(payload);
        b
    }

    #[test]
    fn parses_video_and_audio_tfdt() {
        let mut trafs = Vec::new();
        trafs.extend_from_slice(&wrap_box(
            b"traf",
            &[tfhd_box(1), tfdt_box(0, 10_000)].concat(),
        ));
        trafs.extend_from_slice(&wrap_box(
            b"traf",
            &[tfhd_box(2), tfdt_box(0, 10_500)].concat(),
        ));
        let moof = wrap_box(b"moof", &trafs);
        let bases = parse_av_tfdt_ms(&moof).expect("bases");
        assert_eq!(bases.video_ms, 10_000);
        assert_eq!(bases.audio_ms, 10_500);
        assert_eq!(bases.skew_ms(), 500);
        assert!(exceeds_tolerance(bases.skew_ms(), 499));
        assert!(!exceeds_tolerance(bases.skew_ms(), 500));
    }
}
