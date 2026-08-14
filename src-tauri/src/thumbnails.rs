use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

/// How many frames a preview runs through. Enough to show what a video is —
/// where it was shot, who is in it, whether it is what the name promised —
/// without asking ffmpeg for so many that the first hover has to wait.
pub const PREVIEW_FRAMES: usize = 8;

/// A thumbnail already made by the indexer's background worker, if one is in
/// the shared cache. Reading this never starts ffmpeg, which keeps search
/// results cheap even when the cache is still catching up with a folder.
pub(crate) fn cached(video: &Path) -> Option<PathBuf> {
    let output = cache_output(video);
    output.is_file().then_some(output)
}

/// The cache file for a video. This is crate-visible so search tests can set
/// up the same handoff the background worker makes without invoking ffmpeg.
pub(crate) fn cache_path(video: &Path) -> Option<PathBuf> {
    let cache = cache_dir()?;
    Some(cache.join(format!("{}.jpg", fingerprint(video))))
}

#[derive(Debug, PartialEq, Eq)]
enum GenerationFailure {
    New(String),
    Cached(String),
}

pub fn generate(video: &Path) -> Option<PathBuf> {
    generate_result(video).ok()
}

fn generate_result(video: &Path) -> Result<PathBuf, GenerationFailure> {
    if let Some(output) = cached(video) {
        return Ok(output);
    }
    let output = cache_path(video)
        .ok_or_else(|| GenerationFailure::New("the thumbnail cache could not be created".into()))?;
    let failure = failure_marker(&output);
    if let Some(message) = cached_failure(&failure) {
        return Err(GenerationFailure::Cached(message));
    }
    match extract(video, 1.0, &output, 640) {
        Ok(output) => Ok(output),
        Err(message) => {
            remember_failure(&failure, &message);
            Err(GenerationFailure::New(message))
        }
    }
}

pub(crate) fn generate_folder_with_failures<F>(folder: &Path, mut on_failure: F)
where
    F: FnMut(&Path, &str),
{
    for video in video_paths(folder) {
        if let Err(GenerationFailure::New(message)) = generate_result(&video) {
            on_failure(&video, &message);
        }
    }
}

fn video_paths(folder: &Path) -> Vec<PathBuf> {
    let mut pending = vec![folder.to_path_buf()];
    let mut videos = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && crate::search::is_supported_video(&path)
                && crate::search::is_non_empty_file(&path)
            {
                videos.push(path);
            }
        }
    }
    videos
}

/// Frames sampled evenly across the video, for the preview that runs while the
/// pointer rests on a result. A still says what the first second looked like;
/// these say what the video is.
///
/// All or nothing: a video ffmpeg can seek a ninth of the way into can be
/// seeked eight ninths of the way into too, so a partial set means something is
/// wrong with the file rather than with one offset — and half a preview would
/// be regenerated on every hover, because nothing would tell it apart from a
/// cache that was never filled.
pub fn preview(video: &Path) -> Option<Vec<PathBuf>> {
    let cache = cache_dir()?;
    let key = fingerprint(video);
    let frames: Vec<PathBuf> = (0..PREVIEW_FRAMES)
        .map(|number| cache.join(format!("{key}-preview-{number}.jpg")))
        .collect();
    if frames.iter().all(|frame| frame.is_file()) {
        return Some(frames);
    }
    let failure = failure_marker(&cache.join(format!("{key}.jpg")));
    if cached_failure(&failure).is_some() {
        return None;
    }
    let offsets = match duration(video) {
        Ok(duration) => preview_offsets(duration, PREVIEW_FRAMES),
        Err(message) => {
            remember_failure(&failure, &message);
            return None;
        }
    };
    for (offset, frame) in offsets.into_iter().zip(&frames) {
        if let Err(message) = extract(video, offset, frame, 320) {
            for partial in &frames {
                let _ = fs::remove_file(partial);
            }
            remember_failure(&failure, &message);
            return None;
        }
    }
    Some(frames)
}

/// Where in a `duration`-second video to take `count` frames. Evenly spaced,
/// and inside the video at both ends: the first frame of a video is often a
/// black one, and seeking to the very last is as likely to land past the end as
/// on a picture.
fn preview_offsets(duration: f64, count: usize) -> Vec<f64> {
    (1..=count)
        .map(|step| duration * step as f64 / (count + 1) as f64)
        .collect()
}

fn cache_dir() -> Option<PathBuf> {
    let cache = cache_output_root();
    fs::create_dir_all(&cache).ok()?;
    Some(cache)
}

fn cache_output(video: &Path) -> PathBuf {
    cache_output_root().join(format!("{}.jpg", fingerprint(video)))
}

fn cache_output_root() -> PathBuf {
    std::env::temp_dir().join("toka-thumbnails")
}

fn fingerprint(video: &Path) -> String {
    format!("{:x}", md5_fingerprint(video))
}

/// How long the video runs, so a preview can be spread across it. A file
/// ffprobe cannot read the length of is one Toka leaves showing its still.
fn duration(video: &Path) -> Result<f64, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(video)
        .output()
        .map_err(|error| format!("could not run ffprobe: {error}"))?;
    if !output.status.success() {
        return Err(command_error(&output));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|error| format!("ffprobe returned an invalid duration: {error}"))
        .and_then(|seconds| {
            if seconds.is_finite() && seconds > 0.0 {
                Ok(seconds)
            } else {
                Err("ffprobe returned a non-positive duration".into())
            }
        })
}

/// One frame from `at` seconds in, written whole: ffmpeg is given a temporary
/// name and the result renamed into place, so a reader never finds a half
/// written picture in the cache.
///
/// `-ss` before `-i` is what keeps this quick. After it, ffmpeg decodes the
/// video from the beginning to reach the offset, which for the later frames of
/// a feature-length file is seconds of work each.
fn extract(video: &Path, at: f64, output: &Path, width: u32) -> Result<PathBuf, String> {
    static TEMPORARY_FILES: AtomicU64 = AtomicU64::new(0);
    let temporary = output.with_extension(format!(
        "tmp-{}-{}.jpg",
        std::process::id(),
        TEMPORARY_FILES.fetch_add(1, Ordering::Relaxed)
    ));
    let result = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-ss"])
        .arg(format!("{at:.3}"))
        .arg("-i")
        .arg(video)
        .args(["-frames:v", "1", "-vf"])
        .arg(format!("scale={width}:-2"))
        .args(["-q:v", "5", "-y"])
        .arg(&temporary)
        .output()
        .map_err(|error| format!("could not run ffmpeg: {error}"));
    let output_result = match result {
        Ok(output) => output,
        Err(message) => {
            let _ = fs::remove_file(&temporary);
            return Err(message);
        }
    };
    if !output_result.status.success() {
        let _ = fs::remove_file(&temporary);
        return Err(command_error(&output_result));
    }
    if !temporary.is_file() {
        return Err("ffmpeg succeeded without writing a thumbnail".into());
    }
    if fs::rename(&temporary, output).is_err() {
        let _ = fs::remove_file(&temporary);
        return Err("could not move the generated thumbnail into the cache".into());
    }
    Ok(output.to_path_buf())
}

fn failure_marker(output: &Path) -> PathBuf {
    output.with_extension("failed")
}

fn cached_failure(marker: &Path) -> Option<String> {
    if !marker.is_file() {
        return None;
    }
    Some(
        fs::read_to_string(marker)
            .ok()
            .filter(|message| !message.trim().is_empty())
            .unwrap_or_else(|| "thumbnail generation previously failed".into()),
    )
}

fn remember_failure(marker: &Path, message: &str) {
    let _ = fs::write(marker, message);
}

fn command_error(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("decoder exited with {}", output.status)
    } else {
        stderr
    }
}

fn md5_fingerprint(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.to_string_lossy().hash(&mut hasher);
    if let Ok(metadata) = fs::metadata(path) {
        metadata.len().hash(&mut hasher);
        if let Ok(modified) = metadata.modified() {
            modified.hash(&mut hasher);
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, thread, time::Duration};
    use tempfile::tempdir;

    #[test]
    fn preview_offsets_are_spread_inside_the_video() {
        let offsets = preview_offsets(90.0, 8);

        assert_eq!(offsets.len(), 8);
        // Neither end is sampled: the first frame of a video is often black,
        // and the last is as likely to be past the end as to be a picture.
        assert_eq!(offsets.first(), Some(&10.0));
        assert_eq!(offsets.last(), Some(&80.0));
        assert!(
            offsets.windows(2).all(|pair| pair[1] > pair[0]),
            "{offsets:?}"
        );
    }

    /// A clip shorter than the number of frames asked for still gets that many,
    /// bunched together. Eight near-identical pictures read as the still the
    /// preview replaced, which is the right answer for a video with nothing
    /// else in it.
    #[test]
    fn a_very_short_video_still_gets_a_full_set_of_offsets() {
        let offsets = preview_offsets(0.4, 8);

        assert_eq!(offsets.len(), 8);
        assert!(
            offsets.iter().all(|at| *at > 0.0 && *at < 0.4),
            "{offsets:?}"
        );
    }

    #[test]
    fn background_scanning_finds_supported_videos_recursively() {
        let root = tempdir().unwrap();
        let clip = root.path().join("clip.mp4");
        let empty = root.path().join("empty.mp4");
        let notes = root.path().join("notes.txt");
        let nested = root.path().join("nested");
        let movie = nested.join("movie.MKV");
        fs::write(&clip, b"video").unwrap();
        fs::write(&empty, b"").unwrap();
        fs::write(&notes, b"text").unwrap();
        fs::create_dir(&nested).unwrap();
        fs::write(&movie, b"video").unwrap();

        let mut found = video_paths(root.path());
        found.sort();
        assert_eq!(found, vec![clip, movie]);
    }

    #[test]
    fn failed_thumbnail_generation_is_cached_for_the_current_file() {
        let root = tempdir().unwrap();
        let video = root.path().join("not-a-video.mp4");
        fs::write(&video, b"this is not a video").unwrap();
        let failure = cache_path(&video).unwrap().with_extension("failed");

        assert!(generate(&video).is_none());
        let first_failure = fs::read(&failure).expect("failed thumbnail marker");
        let first_modified = fs::metadata(&failure).unwrap().modified().unwrap();

        thread::sleep(Duration::from_millis(25));
        assert!(generate(&video).is_none());
        assert_eq!(fs::read(&failure).unwrap(), first_failure);
        assert_eq!(
            fs::metadata(&failure).unwrap().modified().unwrap(),
            first_modified
        );

        fs::write(&video, b"a different invalid video").unwrap();
        let changed_failure = cache_path(&video).unwrap().with_extension("failed");
        assert_ne!(changed_failure, failure);
        assert!(generate(&video).is_none());
        assert!(changed_failure.is_file());
    }

    #[test]
    fn cached_thumbnail_failures_are_reported_only_once() {
        let root = tempdir().unwrap();
        let video = root.path().join("not-a-video.mp4");
        fs::write(&video, b"this is not a video").unwrap();
        let failure = cache_path(&video).unwrap().with_extension("failed");
        let _ = fs::remove_file(&failure);
        let mut failures = Vec::new();

        generate_folder_with_failures(root.path(), |path, message| {
            failures.push((path.to_path_buf(), message.to_owned()));
        });
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, video);

        generate_folder_with_failures(root.path(), |path, message| {
            failures.push((path.to_path_buf(), message.to_owned()));
        });
        assert_eq!(failures.len(), 1);
        let _ = fs::remove_file(failure);
    }
}
