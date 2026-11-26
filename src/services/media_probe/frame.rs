//! The preview frame of a video, pulled out with `ffmpeg`.
//!
//! Decoding video is the one part of media handling this crate does not do
//! itself. [`extract_frame`] shells out to `ffmpeg` for the first frame,
//! scaled down so it fits within a bound on both edges, as a `JPEG` the
//! image pipeline of [`media_processing`](crate::services::media_processing)
//! then thumbnails and hashes like any uploaded picture.
//!
//! `ffmpeg` is an optional runtime dependency: when the binary is not on
//! `PATH` — or fails, or runs past its time limit — the answer is `None`
//! and the upload simply has no preview, never an error. The file goes to
//! `ffmpeg` through a temporary file (an `MP4` keeps its index at the end,
//! so it cannot be piped), which is removed when the call returns.

use std::{io::ErrorKind, process::Stdio, time::Duration};

use tempfile::NamedTempFile;
use tokio::{process::Command, time::timeout};
use tracing::{debug, warn};

/// How long `ffmpeg` may spend on one frame before it is killed.
const FRAME_TIMEOUT: Duration = Duration::from_secs(20);

/// The `ffmpeg` filter that scales to fit within `max_edge` on both edges
/// without upscaling, keeping the aspect ratio and even dimensions.
fn scale_filter(max_edge: u32) -> String {
    format!(
        "scale='min({max_edge},iw)':'min({max_edge},ih)':\
         force_original_aspect_ratio=decrease:force_divisible_by=2"
    )
}

/// Writes `bytes` to a fresh temporary file, `None` (logged) when that
/// fails.
async fn write_input(bytes: &[u8]) -> Option<NamedTempFile> {
    let file = match NamedTempFile::new() {
        Ok(file) => file,
        Err(err) => {
            warn!(%err, "no temporary file for the video frame");
            return None;
        }
    };
    if let Err(err) = tokio::fs::write(file.path(), bytes).await {
        warn!(%err, "writing the video to a temporary file failed");
        return None;
    }
    Some(file)
}

/// The latest point of a video, in seconds, its preview frame is taken at.
const MAX_SEEK_SECONDS: f64 = 1.0;

/// Where in a video of `duration` seconds its preview frame is taken.
///
/// The point is the smaller of one second and half the duration, so a clip
/// shorter than two seconds is sampled in its middle. A video with no known
/// (or a useless) duration is sampled at its start.
#[must_use]
pub fn frame_seek(duration: Option<f64>) -> f64 {
    duration
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map_or(0.0, |seconds| (seconds / 2.0).min(MAX_SEEK_SECONDS))
}

/// A frame of the video in `bytes` at `seek_secs`, as `JPEG` bytes scaled
/// to fit within `max_edge` pixels on both edges.
///
/// `program` is the `ffmpeg` binary to run (`FFMPEG_PATH` in production).
/// `seek_secs` is [`frame_seek`] of the video's duration. `None` when the
/// program is not installed, exits unsuccessfully, writes nothing or runs
/// past its time limit; the reason is logged.
pub async fn extract_frame(
    program: &str,
    bytes: &[u8],
    max_edge: u32,
    seek_secs: f64,
) -> Option<Vec<u8>> {
    let input = write_input(bytes).await?;
    let filter = scale_filter(max_edge);
    let seek = format!("{:.3}", seek_secs.max(0.0));
    let ran = timeout(
        FRAME_TIMEOUT,
        Command::new(program)
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-ss"])
            .arg(seek)
            .arg("-i")
            .arg(input.path())
            .args([
                "-frames:v",
                "1",
                "-an",
                "-sn",
                "-vf",
                filter.as_str(),
                "-f",
                "image2pipe",
                "-c:v",
                "mjpeg",
                "-q:v",
                "3",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match ran {
        Err(elapsed) => {
            warn!(%elapsed, program, "extracting the video frame timed out");
            None
        }
        Ok(Err(err)) if err.kind() == ErrorKind::NotFound => {
            debug!(program, "ffmpeg is not installed, storing no video preview");
            None
        }
        Ok(Err(err)) => {
            warn!(%err, program, "running ffmpeg failed");
            None
        }
        Ok(Ok(output)) if output.status.success() && !output.stdout.is_empty() => {
            Some(output.stdout)
        }
        Ok(Ok(output)) => {
            debug!(status = %output.status, program, "ffmpeg produced no frame");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filter_never_upscales_and_keeps_the_aspect() {
        assert_eq!(
            scale_filter(400),
            "scale='min(400,iw)':'min(400,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2",
            "each edge is capped at its own size"
        );
    }

    #[test]
    fn the_frame_is_taken_at_one_second_or_the_middle_of_a_short_clip() {
        for (duration, expected) in [
            (Some(60.0), 1.0),
            (Some(2.0), 1.0),
            (Some(1.5), 0.75),
            (Some(0.2), 0.1),
            (Some(0.0), 0.0),
            (Some(-4.0), 0.0),
            (Some(f64::NAN), 0.0),
            (Some(f64::INFINITY), 0.0),
            (None, 0.0),
        ] {
            assert!(
                (frame_seek(duration) - expected).abs() < 1e-9,
                "{duration:?} seeks to {expected}"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_program_gives_no_frame() {
        assert!(
            extract_frame("wired-be-no-such-ffmpeg", b"video bytes", 400, 1.0)
                .await
                .is_none(),
            "an absent ffmpeg is no preview, not an error"
        );
    }

    #[tokio::test]
    async fn a_failing_or_silent_program_gives_no_frame() {
        for program in ["false", "true"] {
            assert!(
                extract_frame(program, b"video bytes", 400, 1.0)
                    .await
                    .is_none(),
                "`{program}` writes no frame"
            );
        }
    }

    #[tokio::test]
    async fn the_programs_stdout_is_the_frame() {
        let frame = extract_frame("echo", b"video bytes", 400, 0.75)
            .await
            .expect("echo succeeds and prints its arguments");
        let text = String::from_utf8(frame).expect("echo prints text");
        assert!(
            text.starts_with("-nostdin -hide_banner -loglevel error -ss 0.750 -i "),
            "the seek and the input come after the quiet flags: {text}"
        );
        assert!(
            text.contains(&scale_filter(400)) && text.trim_end().ends_with("pipe:1"),
            "the scaling filter is passed and the frame goes to stdout: {text}"
        );
    }
}
