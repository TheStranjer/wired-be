//! Header probing for video and audio uploads: duration and frame size.
//!
//! Mastodon clients size the player and compute the aspect ratio from the
//! attachment's `meta.original`, so an upload has to record what the file
//! says about itself. [`probe`] reads that from the container headers in
//! pure Rust — no external tool, no decoding: an `ISO BMFF` file
//! (`MP4`, `M4A`, `MOV`) states its duration in `moov/mvhd` and its
//! presentation size in the video track's `tkhd`; an `MP3` states its
//! length in a `Xing`/`Info` frame count or, failing that, follows from its
//! constant bitrate. Containers it does not read (`WebM`, `Ogg`,
//! …) probe to nothing and the row simply keeps those columns unknown.
//!
//! The one thing that does need a decoder is the preview frame of a video,
//! which `extract_frame` pulls out with `ffmpeg` when the binary is on `PATH`.
//!
//! Every function here is pure over the bytes it is given (the frame
//! extractor excepted) and never fails: a truncated or hostile header is
//! `None`, not a panic and not an upload rejection.

mod frame;
mod mp3;

pub use frame::{FFMPEG, extract_frame};

/// What a container's headers say about its media.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MediaProbe {
    /// `(width, height)` in pixels of the first video track; `None` for
    /// audio and for files whose size could not be read.
    pub dimensions: Option<(u32, u32)>,
    /// Length in seconds; `None` when the file does not state one.
    pub duration: Option<f64>,
}

/// The `MIME` types an `ISO BMFF` file is detected as.
const MP4_CONTENT_TYPES: [&str; 6] = [
    "video/mp4",
    "video/quicktime",
    "video/x-m4v",
    "audio/mp4",
    "audio/m4a",
    "audio/x-m4a",
];

/// The `MIME` type `MP3` files carry.
const MP3_CONTENT_TYPE: &str = "audio/mpeg";

/// Reads `bytes`, a file detected as `content_type`, for its duration and
/// frame size.
///
/// Unknown containers and unreadable headers give the empty
/// [`MediaProbe`].
#[must_use]
pub fn probe(bytes: &[u8], content_type: &str) -> MediaProbe {
    if MP4_CONTENT_TYPES.contains(&content_type) {
        probe_mp4(bytes)
    } else if content_type == MP3_CONTENT_TYPE {
        mp3::probe(bytes)
    } else {
        MediaProbe::default()
    }
}

/// One `ISO BMFF` box: its four-character type and its payload.
#[derive(Clone, Copy, Debug)]
struct BoxRef<'a> {
    /// The four-character box type, e.g. `moov`.
    kind: [u8; 4],
    /// Everything after the box header.
    payload: &'a [u8],
}

/// A big-endian `u32` at `at`, `None` when the slice is too short.
fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    data.get(at..end)?.try_into().ok().map(u32::from_be_bytes)
}

/// A big-endian `u64` at `at`, `None` when the slice is too short.
fn be_u64(data: &[u8], at: usize) -> Option<u64> {
    let end = at.checked_add(8)?;
    data.get(at..end)?.try_into().ok().map(u64::from_be_bytes)
}

/// `value` as a float, exact up to 2^53 (a `u64` has no lossless `f64`
/// conversion, so it goes through its two halves).
fn u64_to_f64(value: u64) -> f64 {
    let high = f64::from(u32::try_from(value >> 32).unwrap_or(u32::MAX));
    let low = f64::from(u32::try_from(value & 0xFFFF_FFFF).unwrap_or(u32::MAX));
    high.mul_add(4_294_967_296.0, low)
}

/// Splits the first box off `data`: the box and what follows it. `None`
/// for a truncated or impossible header, which ends iteration.
fn next_box(data: &[u8]) -> Option<(BoxRef<'_>, &[u8])> {
    let size = u64::from(be_u32(data, 0)?);
    let kind: [u8; 4] = data.get(4..8)?.try_into().ok()?;
    let (header_len, total) = match size {
        0 => (8, data.len()),
        1 => (16, usize::try_from(be_u64(data, 8)?).ok()?),
        _ => (8, usize::try_from(size).ok()?),
    };
    if total < header_len {
        return None;
    }
    let (whole, rest) = data.split_at_checked(total)?;
    let payload = whole.get(header_len..)?;
    Some((BoxRef { kind, payload }, rest))
}

/// The sibling boxes in `data`, in order.
fn boxes(data: &[u8]) -> impl Iterator<Item = BoxRef<'_>> {
    let mut remaining = data;
    std::iter::from_fn(move || {
        let (found, rest) = next_box(remaining)?;
        remaining = rest;
        Some(found)
    })
}

/// The payload of the first `kind` box among `data`'s siblings.
fn child(data: &[u8], kind: [u8; 4]) -> Option<&[u8]> {
    boxes(data)
        .find(|candidate| candidate.kind == kind)
        .map(|found| found.payload)
}

/// The duration in seconds a `mvhd` payload states: its duration over its
/// timescale. `None` for a zero timescale or a non-positive length.
fn movie_duration(mvhd: &[u8]) -> Option<f64> {
    let (scale_at, length) = if *mvhd.first()? == 1 {
        (20, be_u64(mvhd, 24)?)
    } else {
        (12, u64::from(be_u32(mvhd, 16)?))
    };
    let scale = be_u32(mvhd, scale_at)?;
    if scale == 0 || length == 0 {
        return None;
    }
    Some(u64_to_f64(length) / f64::from(scale))
}

/// The `(width, height)` a `trak` payload's `tkhd` states, `None` for a
/// track without a picture (audio tracks state `0x0`).
///
/// The sizes are 16.16 fixed-point; only the whole part is kept.
fn track_dimensions(trak: &[u8]) -> Option<(u32, u32)> {
    let tkhd = child(trak, *b"tkhd")?;
    let at = if *tkhd.first()? == 1 { 88 } else { 76 };
    let width = be_u32(tkhd, at)? >> 16;
    let height = be_u32(tkhd, at.checked_add(4)?)? >> 16;
    (width > 0 && height > 0).then_some((width, height))
}

/// Probes an `ISO BMFF` file: the movie duration and the first picture
/// track's size, both from `moov`.
fn probe_mp4(bytes: &[u8]) -> MediaProbe {
    let Some(moov) = child(bytes, *b"moov") else {
        return MediaProbe::default();
    };
    let duration = child(moov, *b"mvhd").and_then(movie_duration);
    let dimensions = boxes(moov)
        .filter(|candidate| candidate.kind == *b"trak")
        .find_map(|track| track_dimensions(track.payload));
    MediaProbe {
        dimensions,
        duration,
    }
}

/// Hand-built `ISO BMFF` fixtures for this module's tests: a few dozen
/// bytes each, no decodable media.
#[cfg(test)]
mod test_support {
    /// One box: size, type, payload.
    pub(super) fn mp4_box(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(payload.len() + 8).expect("a fixture box is small");
        let mut out = size.to_be_bytes().to_vec();
        out.extend_from_slice(&kind);
        out.extend_from_slice(payload);
        out
    }

    /// A version-0 `mvhd` payload stating `duration` units of `timescale`.
    pub(super) fn mvhd(timescale: u32, duration: u32) -> Vec<u8> {
        let mut payload = vec![0; 100];
        payload[12..16].copy_from_slice(&timescale.to_be_bytes());
        payload[16..20].copy_from_slice(&duration.to_be_bytes());
        mp4_box(*b"mvhd", &payload)
    }

    /// A version-0 `tkhd` payload for a `width` x `height` track.
    pub(super) fn tkhd(width: u32, height: u32) -> Vec<u8> {
        let mut payload = vec![0; 84];
        payload[76..80].copy_from_slice(&(width << 16).to_be_bytes());
        payload[80..84].copy_from_slice(&(height << 16).to_be_bytes());
        mp4_box(*b"tkhd", &payload)
    }

    /// A minimal `ftyp` + `moov` file with an audio track (no picture)
    /// then a `width` x `height` video track, `seconds` long.
    pub(super) fn mp4_file(width: u32, height: u32, seconds: u32) -> Vec<u8> {
        let mut moov = mvhd(1000, seconds * 1000);
        moov.extend(mp4_box(*b"trak", &tkhd(0, 0)));
        moov.extend(mp4_box(*b"trak", &tkhd(width, height)));
        let mut file = mp4_box(*b"ftyp", b"mp42\x00\x00\x00\x00mp42isom");
        file.extend(mp4_box(*b"mdat", &[0; 16]));
        file.extend(mp4_box(*b"moov", &moov));
        file
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{mp4_box, mp4_file, mvhd, tkhd};
    use super::*;

    #[test]
    fn an_mp4_states_its_size_and_duration_in_the_moov() {
        let file = mp4_file(640, 360, 3);
        assert_eq!(
            probe(&file, "video/mp4"),
            MediaProbe {
                dimensions: Some((640, 360)),
                duration: Some(3.0),
            },
            "the picture track's size and mvhd's seconds, past mdat and the audio track"
        );
    }

    #[test]
    fn quicktime_and_m4a_labels_read_the_same_container() {
        let file = mp4_file(320, 240, 1);
        for content_type in ["video/quicktime", "audio/mp4", "audio/x-m4a"] {
            assert_eq!(
                probe(&file, content_type).dimensions,
                Some((320, 240)),
                "{content_type} is an ISO BMFF file"
            );
        }
    }

    #[test]
    fn an_audio_only_mp4_has_a_duration_and_no_size() {
        let mut moov = mvhd(44_100, 88_200);
        moov.extend(mp4_box(*b"trak", &tkhd(0, 0)));
        let mut file = mp4_box(*b"ftyp", b"M4A \x00\x00\x00\x00M4A mp42");
        file.extend(mp4_box(*b"moov", &moov));
        assert_eq!(
            probe(&file, "audio/mp4"),
            MediaProbe {
                dimensions: None,
                duration: Some(2.0),
            },
            "two seconds, no picture"
        );
    }

    #[test]
    fn a_version_one_mvhd_reads_its_wide_fields() {
        let mut payload = vec![0; 120];
        payload[0] = 1;
        payload[20..24].copy_from_slice(&1000_u32.to_be_bytes());
        payload[24..32].copy_from_slice(&90_000_u64.to_be_bytes());
        assert_eq!(
            movie_duration(&payload),
            Some(90.0),
            "64-bit duration after a 64-bit creation and modification time"
        );
    }

    #[test]
    fn a_version_one_tkhd_reads_its_size_at_the_later_offset() {
        let mut payload = vec![0; 96];
        payload[0] = 1;
        payload[88..92].copy_from_slice(&(1920_u32 << 16).to_be_bytes());
        payload[92..96].copy_from_slice(&(1080_u32 << 16).to_be_bytes());
        let track = mp4_box(*b"tkhd", &payload);
        assert_eq!(track_dimensions(&track), Some((1920, 1080)), "full HD");
    }

    #[test]
    fn a_zero_timescale_or_length_is_no_duration() {
        assert_eq!(
            probe(&mp4_file(8, 8, 0), "video/mp4").duration,
            None,
            "a zero length states nothing"
        );
        let mut moov = mvhd(0, 5);
        moov.extend(mp4_box(*b"trak", &tkhd(8, 8)));
        let file = mp4_box(*b"moov", &moov);
        let probed = probe(&file, "video/mp4");
        assert_eq!(probed.duration, None, "a zero timescale cannot divide");
        assert_eq!(probed.dimensions, Some((8, 8)), "the size still reads");
    }

    #[test]
    fn truncated_hostile_or_foreign_bytes_probe_to_nothing() {
        let file = mp4_file(640, 360, 3);
        let cut = file.get(..file.len() - 10).expect("the fixture is longer");
        for (label, bytes) in [
            ("a moov cut short", cut),
            ("no bytes", b"".as_slice()),
            (
                "an ftyp alone",
                b"\x00\x00\x00\x10ftypmp42\x00\x00\x00\x00".as_slice(),
            ),
            (
                "a box smaller than its header",
                b"\x00\x00\x00\x04moov".as_slice(),
            ),
            (
                "a box larger than the file",
                b"\xff\xff\xff\xffmoovxxxx".as_slice(),
            ),
            (
                "a 64-bit size that cannot fit",
                b"\x00\x00\x00\x01moov\xff\xff\xff\xff\xff\xff\xff\xff".as_slice(),
            ),
        ] {
            assert_eq!(
                probe(bytes, "video/mp4"),
                MediaProbe::default(),
                "{label} gives the empty probe"
            );
        }
    }

    #[test]
    fn a_container_it_does_not_read_probes_to_nothing() {
        let file = mp4_file(640, 360, 3);
        for content_type in ["video/webm", "audio/ogg", "image/png", ""] {
            assert_eq!(
                probe(&file, content_type),
                MediaProbe::default(),
                "`{content_type}` is not parsed"
            );
        }
    }
}
