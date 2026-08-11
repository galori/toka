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

pub fn generate(video: &Path) -> Option<PathBuf> {
    if let Some(output) = cached(video) {
        return Some(output);
    }
    let output = cache_path(video)?;
    extract(video, 1.0, &output, 640)
}

/// Fill the shared cache for every supported video below `folder`. The
/// caller owns the worker thread, so this deliberately processes one folder
/// serially and lets the indexer continue watching while ffmpeg does the
/// expensive work.
pub fn generate_folder(folder: &Path) {
    for video in video_paths(folder) {
        let _ = generate(&video);
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
            } else if file_type.is_file() && crate::search::is_supported_video(&path) {
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
    let offsets = preview_offsets(duration(video)?, PREVIEW_FRAMES);
    for (offset, frame) in offsets.into_iter().zip(&frames) {
        extract(video, offset, frame, 320)?;
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
fn duration(video: &Path) -> Option<f64> {
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
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
}

/// One frame from `at` seconds in, written whole: ffmpeg is given a temporary
/// name and the result renamed into place, so a reader never finds a half
/// written picture in the cache.
///
/// `-ss` before `-i` is what keeps this quick. After it, ffmpeg decodes the
/// video from the beginning to reach the offset, which for the later frames of
/// a feature-length file is seconds of work each.
fn extract(video: &Path, at: f64, output: &Path, width: u32) -> Option<PathBuf> {
    static TEMPORARY_FILES: AtomicU64 = AtomicU64::new(0);
    let temporary = output.with_extension(format!(
        "tmp-{}-{}.jpg",
        std::process::id(),
        TEMPORARY_FILES.fetch_add(1, Ordering::Relaxed)
    ));
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-ss"])
        .arg(format!("{at:.3}"))
        .arg("-i")
        .arg(video)
        .args(["-frames:v", "1", "-vf"])
        .arg(format!("scale={width}:-2"))
        .args(["-q:v", "5", "-y"])
        .arg(&temporary)
        .status()
        .ok()?;
    if !status.success() || !temporary.is_file() {
        let _ = fs::remove_file(&temporary);
        return None;
    }
    if fs::rename(&temporary, output).is_err() {
        let _ = fs::remove_file(&temporary);
        return None;
    }
    Some(output.to_path_buf())
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
    use std::fs;
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
        let notes = root.path().join("notes.txt");
        let nested = root.path().join("nested");
        let movie = nested.join("movie.MKV");
        fs::write(&clip, b"video").unwrap();
        fs::write(&notes, b"text").unwrap();
        fs::create_dir(&nested).unwrap();
        fs::write(&movie, b"video").unwrap();

        let mut found = video_paths(root.path());
        found.sort();
        assert_eq!(found, vec![clip, movie]);
    }
}
