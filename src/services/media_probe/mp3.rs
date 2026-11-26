//! `MP3` probing: the length of an MPEG audio Layer III file.
//!
//! An `MP3` has no container header, so its length is worked out from its
//! first frame: a `Xing`/`Info` tag there states the frame count exactly
//! (variable-bitrate encoders write it); without one, the file is taken to
//! be constant-bitrate and its length follows from the audio byte count
//! and the first frame's bitrate. A leading `ID3v2` tag and a trailing
//! `ID3v1` tag are not audio and are left out of that count.

use super::{MediaProbe, be_u32, u64_to_f64};

/// How many bytes past the tag the first frame header is searched for.
const SCAN_LIMIT: usize = 16 * 1024;

/// Layer III bitrates in kbit/s for `MPEG-1`, by header index; index 0 is
/// the unsupported free format.
const BITRATES_V1: [u32; 15] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
];

/// Layer III bitrates in kbit/s for `MPEG-2` and `MPEG-2.5`, by header
/// index.
const BITRATES_V2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

/// Sample rates by version row (`MPEG-1`, `MPEG-2`, `MPEG-2.5`) and header
/// index.
const SAMPLE_RATES: [[u32; 3]; 3] = [
    [44_100, 48_000, 32_000],
    [22_050, 24_000, 16_000],
    [11_025, 12_000, 8_000],
];

/// The size of a trailing `ID3v1` tag.
const ID3V1_LEN: usize = 128;

/// What the first frame header says.
#[derive(Clone, Copy, Debug)]
struct FrameHeader {
    /// Bitrate in kbit/s.
    kbps: u32,
    /// Sample rate in hertz.
    sample_rate: u32,
    /// Samples per frame: 1152 for `MPEG-1`, 576 below.
    samples: u32,
    /// Where a `Xing`/`Info` tag would start, from the frame's start.
    xing_offset: usize,
}

/// The header `word` describes, `None` for anything but a Layer III frame
/// with a fixed bitrate and a real sample rate.
fn parse_header(word: u32) -> Option<FrameHeader> {
    if word >> 21 != 0x7FF {
        return None;
    }
    let version = (word >> 19) & 0b11;
    if ((word >> 17) & 0b11) != 1 || version == 1 {
        return None;
    }
    let is_v1 = version == 3;
    let bitrates = if is_v1 { &BITRATES_V1 } else { &BITRATES_V2 };
    let kbps = *bitrates.get(usize::try_from((word >> 12) & 0xF).ok()?)?;
    let row = match version {
        3 => 0,
        2 => 1,
        _ => 2,
    };
    let rate_index = usize::try_from((word >> 10) & 0b11).ok()?;
    let sample_rate = *SAMPLE_RATES.get(row)?.get(rate_index)?;
    if kbps == 0 {
        return None;
    }
    let mono = ((word >> 6) & 0b11) == 3;
    let side_info = match (is_v1, mono) {
        (true, false) => 32,
        (false, true) => 9,
        _ => 17,
    };
    let crc = if ((word >> 16) & 1) == 0 { 2 } else { 0 };
    Some(FrameHeader {
        kbps,
        sample_rate,
        samples: if is_v1 { 1152 } else { 576 },
        xing_offset: 4 + crc + side_info,
    })
}

/// The size of a leading `ID3v2` tag, header and footer included; `0` when
/// the file does not start with one.
fn id3v2_len(bytes: &[u8]) -> usize {
    if !bytes.starts_with(b"ID3") {
        return 0;
    }
    let Some(size_bytes) = bytes.get(6..10) else {
        return 0;
    };
    let size = size_bytes
        .iter()
        .fold(0_usize, |acc, byte| (acc << 7) | usize::from(*byte & 0x7F));
    let has_footer = bytes.get(5).is_some_and(|flags| (*flags & 0x10) != 0);
    10 + size + if has_footer { 10 } else { 0 }
}

/// The offset and header of the first frame at or after `start`.
fn find_frame(bytes: &[u8], start: usize) -> Option<(usize, FrameHeader)> {
    let end = start.saturating_add(SCAN_LIMIT).min(bytes.len());
    (start..end).find_map(|at| Some((at, parse_header(be_u32(bytes, at)?)?)))
}

/// The audio frame count a `Xing`/`Info` tag in the frame at `frame_at`
/// states, `None` when there is no such tag or it carries no count.
fn xing_frames(bytes: &[u8], frame_at: usize, header: FrameHeader) -> Option<u32> {
    let tag_at = frame_at.checked_add(header.xing_offset)?;
    let tag = bytes.get(tag_at..tag_at.checked_add(4)?)?;
    if tag != b"Xing" && tag != b"Info" {
        return None;
    }
    let flags = be_u32(bytes, tag_at.checked_add(4)?)?;
    if (flags & 1) == 0 {
        return None;
    }
    be_u32(bytes, tag_at.checked_add(8)?).filter(|frames| *frames > 0)
}

/// The length in seconds of the audio from `frame_at` to the end of the
/// file at the first frame's constant bitrate.
fn constant_bitrate_seconds(bytes: &[u8], frame_at: usize, header: FrameHeader) -> f64 {
    let has_id3v1 = bytes
        .len()
        .checked_sub(ID3V1_LEN)
        .and_then(|from| bytes.get(from..from.checked_add(3)?))
        .is_some_and(|tag| tag == b"TAG");
    let trailer = if has_id3v1 { ID3V1_LEN } else { 0 };
    let audio_bytes = bytes.len().saturating_sub(frame_at).saturating_sub(trailer);
    let bits = u64::try_from(audio_bytes).unwrap_or(0).saturating_mul(8);
    u64_to_f64(bits) / (f64::from(header.kbps) * 1000.0)
}

/// Probes an `MP3` file for its length; an `MP3` states no picture size.
pub(super) fn probe(bytes: &[u8]) -> MediaProbe {
    let Some((frame_at, header)) = find_frame(bytes, id3v2_len(bytes)) else {
        return MediaProbe::default();
    };
    let seconds = xing_frames(bytes, frame_at, header).map_or_else(
        || constant_bitrate_seconds(bytes, frame_at, header),
        |frames| f64::from(frames) * f64::from(header.samples) / f64::from(header.sample_rate),
    );
    MediaProbe {
        dimensions: None,
        duration: (seconds > 0.0).then_some(seconds),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `MPEG-1` Layer III, 128 kbit/s, 44.1 kHz, stereo frame header.
    const HEADER: [u8; 4] = [0xFF, 0xFB, 0x90, 0x00];
    /// The byte length of one such frame.
    const FRAME_LEN: usize = 417;

    /// `frames` silent frames of [`FRAME_LEN`] bytes.
    fn frames(count: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..count {
            let mut frame = vec![0; FRAME_LEN];
            frame[..4].copy_from_slice(&HEADER);
            out.extend(frame);
        }
        out
    }

    /// Whether `seconds` is within a microsecond of `expected`.
    fn close(seconds: Option<f64>, expected: f64) -> bool {
        seconds.is_some_and(|value| (value - expected).abs() < 1e-6)
    }

    #[test]
    fn a_constant_bitrate_file_is_measured_by_its_size() {
        let mut file = b"ID3\x03\x00\x00\x00\x00\x00\x00".to_vec();
        file.extend(frames(100));
        assert!(
            close(probe(&file).duration, 2.60625),
            "41700 audio bytes at 128 kbit/s, the ID3v2 tag left out"
        );
        assert_eq!(probe(&file).dimensions, None, "audio has no picture");
    }

    #[test]
    fn an_id3v1_trailer_is_not_audio() {
        let mut file = frames(100);
        let mut trailer = vec![0; ID3V1_LEN];
        trailer[..3].copy_from_slice(b"TAG");
        file.extend(trailer);
        assert!(
            close(probe(&file).duration, 2.60625),
            "the 128-byte tag is subtracted"
        );
    }

    #[test]
    fn a_xing_tag_states_the_frame_count() {
        let mut file = frames(2);
        file[36..40].copy_from_slice(b"Xing");
        file[40..44].copy_from_slice(&1_u32.to_be_bytes());
        file[44..48].copy_from_slice(&100_u32.to_be_bytes());
        assert!(
            close(probe(&file).duration, 100.0 * 1152.0 / 44_100.0),
            "100 frames of 1152 samples at 44.1 kHz, whatever the file's size"
        );
    }

    #[test]
    fn a_xing_tag_without_a_frame_count_falls_back_to_the_size() {
        let mut file = frames(100);
        file[36..40].copy_from_slice(b"Info");
        assert!(
            close(probe(&file).duration, 2.60625),
            "no frames flag, so the constant-bitrate estimate"
        );
    }

    #[test]
    fn the_first_frame_may_follow_some_junk() {
        let mut file = vec![0; 100];
        file.extend(frames(10));
        assert!(
            close(probe(&file).duration, 4170.0 * 8.0 / 128_000.0),
            "the scan finds the header past the junk and counts from there"
        );
    }

    #[test]
    fn only_layer_three_with_a_real_rate_is_read() {
        for (label, word) in [
            ("layer II", 0xFFFD_9000_u32),
            ("reserved version", 0xFFEB_9000),
            ("free format", 0xFFFB_0000),
            ("bad bitrate index", 0xFFFB_F000),
            ("reserved sample rate", 0xFFFB_9C00),
            ("no sync", 0x7FFB_9000),
        ] {
            assert!(parse_header(word).is_none(), "{label} is not a frame");
        }
        let v2 = parse_header(0xFFF3_8000).expect("MPEG-2 Layer III, 64 kbit/s... at 22.05 kHz");
        assert_eq!(
            (v2.kbps, v2.sample_rate, v2.samples),
            (64, 22_050, 576),
            "the lower sampling-frequency tables"
        );
    }

    #[test]
    fn bytes_without_a_frame_probe_to_nothing() {
        let zeros = [0_u8; 64];
        for bytes in [
            b"".as_slice(),
            b"ID3".as_slice(),
            zeros.as_slice(),
            b"no audio here".as_slice(),
        ] {
            assert_eq!(
                probe(bytes),
                MediaProbe::default(),
                "{} bytes without a frame header",
                bytes.len()
            );
        }
    }

    #[test]
    fn an_id3v2_size_is_syncsafe_and_counts_its_footer() {
        let plain = b"ID3\x04\x00\x00\x00\x00\x02\x01".to_vec();
        assert_eq!(id3v2_len(&plain), 10 + 257, "two 7-bit groups: 2*128 + 1");
        let footer = b"ID3\x04\x00\x10\x00\x00\x00\x05".to_vec();
        assert_eq!(id3v2_len(&footer), 10 + 5 + 10, "the footer flag adds ten");
        assert_eq!(id3v2_len(b"RIFF"), 0, "no tag, no offset");
    }
}
