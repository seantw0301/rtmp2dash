//! Extract the first `traf` media duration (ms @ timescale 1000) from a CMAF `m4s`.

/// Sum of sample durations in the first `traf` of `moof`, in the track timescale.
///
/// Returns `None` when the fragment cannot be parsed. Callers should fall back to
/// the configured target segment duration.
pub fn first_traf_duration_ticks(data: &[u8]) -> Option<u64> {
    let moof = find_box(data, b"moof")?;
    let mut pos = 0usize;
    while pos < moof.len() {
        let (hdr, payload) = read_box_at(moof, pos)?;
        if &hdr.typ == b"traf" {
            return traf_duration_ticks(payload);
        }
        pos = hdr.end;
    }
    None
}

/// `baseMediaDecodeTime` of the first `traf`'s `tfdt`, in the track timescale.
///
/// This is the segment's absolute media start time since the Segmenter began,
/// used as the `t` attribute in the MPD `SegmentTimeline`.
pub fn first_tfdt_base_time(data: &[u8]) -> Option<u64> {
    let moof = find_box(data, b"moof")?;
    let traf = find_box(moof, b"traf")?;
    let tfdt = find_box(traf, b"tfdt")?;
    if tfdt.len() < 8 {
        return None;
    }
    let version = tfdt[0];
    if version == 1 {
        if tfdt.len() < 12 {
            return None;
        }
        Some(u64::from_be_bytes(tfdt[4..12].try_into().ok()?))
    } else {
        Some(u32::from_be_bytes(tfdt[4..8].try_into().ok()?) as u64)
    }
}

fn traf_duration_ticks(traf: &[u8]) -> Option<u64> {
    let mut default_sample_duration: Option<u32> = None;
    let mut pos = 0usize;
    while pos < traf.len() {
        let (hdr, payload) = read_box_at(traf, pos)?;
        match &hdr.typ {
            b"tfhd" => {
                default_sample_duration = parse_tfhd_default_duration(payload);
            }
            b"trun" => {
                return Some(parse_trun_duration(payload, default_sample_duration)?);
            }
            _ => {}
        }
        pos = hdr.end;
    }
    None
}

fn parse_tfhd_default_duration(payload: &[u8]) -> Option<u32> {
    if payload.len() < 8 {
        return None;
    }
    let flags = u32::from_be_bytes(payload[0..4].try_into().ok()?) & 0x00FF_FFFF;
    // skip version/flags (4) + track_id (4)
    let mut off = 8usize;
    // base-data-offset-present
    if flags & 0x000001 != 0 {
        off = off.checked_add(8)?;
    }
    // sample-description-index-present
    if flags & 0x000002 != 0 {
        off = off.checked_add(4)?;
    }
    // default-sample-duration-present
    if flags & 0x000008 != 0 {
        if payload.len() < off + 4 {
            return None;
        }
        return Some(u32::from_be_bytes(payload[off..off + 4].try_into().ok()?));
    }
    None
}

fn parse_trun_duration(payload: &[u8], default_sample_duration: Option<u32>) -> Option<u64> {
    if payload.len() < 8 {
        return None;
    }
    let version_flags = u32::from_be_bytes(payload[0..4].try_into().ok()?);
    let flags = version_flags & 0x00FF_FFFF;
    let sample_count = u32::from_be_bytes(payload[4..8].try_into().ok()?) as usize;
    let mut off = 8usize;
    // data-offset-present
    if flags & 0x000001 != 0 {
        off = off.checked_add(4)?;
    }
    // first-sample-flags-present
    if flags & 0x000004 != 0 {
        off = off.checked_add(4)?;
    }
    let has_duration = flags & 0x000100 != 0;
    let has_size = flags & 0x000200 != 0;
    let has_flags = flags & 0x000400 != 0;
    let has_cto = flags & 0x000800 != 0;
    let mut per = 0usize;
    if has_duration {
        per += 4;
    }
    if has_size {
        per += 4;
    }
    if has_flags {
        per += 4;
    }
    if has_cto {
        per += 4;
    }

    if !has_duration {
        let d = default_sample_duration? as u64;
        return Some(d.saturating_mul(sample_count as u64));
    }
    if per == 0 || payload.len() < off.checked_add(per.checked_mul(sample_count)?)? {
        return None;
    }

    let mut total = 0u64;
    for i in 0..sample_count {
        let start = off + i * per;
        let dur = u32::from_be_bytes(payload[start..start + 4].try_into().ok()?) as u64;
        total = total.saturating_add(dur);
    }
    Some(total)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal moof/traf/tfhd/trun with two samples of duration 1000.
    #[test]
    fn parses_trun_sample_durations() {
        let mut moof = Vec::new();
        // moof size placeholder
        let moof_start = moof.len();
        moof.extend_from_slice(&0u32.to_be_bytes());
        moof.extend_from_slice(b"moof");

        let traf_start = moof.len();
        moof.extend_from_slice(&0u32.to_be_bytes());
        moof.extend_from_slice(b"traf");

        // tfhd: version/flags=0, track_id=1 (no default duration)
        let tfhd_start = moof.len();
        moof.extend_from_slice(&0u32.to_be_bytes());
        moof.extend_from_slice(b"tfhd");
        moof.extend_from_slice(&0u32.to_be_bytes()); // ver/flags
        moof.extend_from_slice(&1u32.to_be_bytes()); // track_id
        let tfhd_size = (moof.len() - tfhd_start) as u32;
        moof[tfhd_start..tfhd_start + 4].copy_from_slice(&tfhd_size.to_be_bytes());

        // trun: flags sample-duration-present (0x100), sample_count=2, d=1000,1000
        let trun_start = moof.len();
        moof.extend_from_slice(&0u32.to_be_bytes());
        moof.extend_from_slice(b"trun");
        moof.extend_from_slice(&0x00000100u32.to_be_bytes());
        moof.extend_from_slice(&2u32.to_be_bytes());
        moof.extend_from_slice(&1000u32.to_be_bytes());
        moof.extend_from_slice(&1000u32.to_be_bytes());
        let trun_size = (moof.len() - trun_start) as u32;
        moof[trun_start..trun_start + 4].copy_from_slice(&trun_size.to_be_bytes());

        let traf_size = (moof.len() - traf_start) as u32;
        moof[traf_start..traf_start + 4].copy_from_slice(&traf_size.to_be_bytes());
        let moof_size = (moof.len() - moof_start) as u32;
        moof[moof_start..moof_start + 4].copy_from_slice(&moof_size.to_be_bytes());

        assert_eq!(first_traf_duration_ticks(&moof), Some(2000));
    }

    fn tfdt_box(version: u8, base_time: u64) -> Vec<u8> {
        let mut b = Vec::new();
        let start = b.len();
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(b"tfdt");
        b.push(version);
        b.extend_from_slice(&[0, 0, 0]); // flags
        if version == 1 {
            b.extend_from_slice(&base_time.to_be_bytes());
        } else {
            b.extend_from_slice(&(base_time as u32).to_be_bytes());
        }
        let size = (b.len() - start) as u32;
        b[start..start + 4].copy_from_slice(&size.to_be_bytes());
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
    fn parses_tfdt_base_media_decode_time() {
        for version in [0u8, 1u8] {
            let tfdt = tfdt_box(version, 96_000);
            let traf = wrap_box(b"traf", &tfdt);
            let moof = wrap_box(b"moof", &traf);
            assert_eq!(first_tfdt_base_time(&moof), Some(96_000), "v{version}");
        }
    }
}
