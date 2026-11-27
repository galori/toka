//! Video duration probing, shared by thumbnail previews and index
//! enrichment. ffprobe reads the length; Toka never decodes the file
//! itself.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::SystemTime,
};

/// How long the video runs, in seconds, so a preview can be spread across
/// it. A file ffprobe cannot read the length of is one Toka leaves showing
/// its still.
pub(crate) fn probe_duration_seconds(video: &Path) -> Result<f64, String> {
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
    parse_duration_seconds(String::from_utf8_lossy(&output.stdout).trim())
}

/// Parses ffprobe's `format=duration` output: a bare second count.
/// Anything missing or not a positive finite length is not a length.
fn parse_duration_seconds(text: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .map_err(|error| format!("ffprobe returned an invalid duration: {error}"))
        .and_then(|seconds| {
            if seconds.is_finite() && seconds > 0.0 {
                Ok(seconds)
            } else {
                Err("ffprobe returned a non-positive duration".into())
            }
        })
}

/// Whole milliseconds for a probe's second count, for the index.
pub(crate) fn seconds_to_ms(seconds: f64) -> u64 {
    (seconds * 1000.0).round() as u64
}

/// Every non-empty supported video under `folder`, recursively. Shared by
/// thumbnail generation and duration enrichment so the two passes always
/// agree on what a video is.
pub(crate) fn video_paths(folder: &Path) -> Vec<PathBuf> {
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

/// Videos whose probe already failed, keyed by path, size, and mtime, so a
/// corrupt file is probed once per lifetime instead of once per pass — but
/// a changed file is probed again. Bounded: past ten thousand entries the
/// set clears and every failure gets one more probe, which caps tagging
/// churn at the cost of an occasional re-probe. Shared by the enrichment
/// worker and the supervisor's backfill: the backfill must not re-enqueue
/// folders whose videos all failed, or the queue would never drain and the
/// daemon would never report idle.
#[derive(Debug, Default)]
pub(crate) struct ProbeFailures {
    failed: Mutex<HashSet<(PathBuf, u64, SystemTime)>>,
}

impl ProbeFailures {
    pub(crate) fn contains(&self, path: &Path, size: u64, modified: SystemTime) -> bool {
        self.failed
            .lock()
            .unwrap()
            .contains(&(path.to_path_buf(), size, modified))
    }

    fn insert(&self, path: &Path, size: u64, modified: SystemTime) {
        let mut failed = self.failed.lock().unwrap();
        if failed.len() >= 10_000 {
            failed.clear();
        }
        failed.insert((path.to_path_buf(), size, modified));
    }
}

/// Probes every video under `folder` that still lacks a duration and
/// records the answers. Videos already probed are never touched; probe
/// failures report through `on_failure` and stay unknown for a later
/// pass. A file that changes mid-probe keeps its unknown duration: the
/// conditional write loses to the rescan's fresh row.
pub(crate) fn enrich_folder_durations<F, G>(
    database: &Path,
    folder: &Path,
    failures: &ProbeFailures,
    prober: F,
    mut on_failure: G,
) where
    F: Fn(&Path) -> Result<u64, String>,
    G: FnMut(&Path, &str),
{
    let index = match crate::owned_index::OwnedIndex::open(database) {
        Ok(index) => index,
        Err(error) => {
            on_failure(folder, &error.to_string());
            return;
        }
    };
    let videos = video_paths(folder);
    let known = index.durations_for(&videos).unwrap_or_default();
    for video in videos {
        if known.contains_key(&video) {
            continue;
        }
        // Gone or unreadable mid-walk: the next scan drops the row, and
        // there is nothing to probe meanwhile.
        let Ok(metadata) = fs::metadata(&video) else {
            continue;
        };
        let size = metadata.len();
        let Some(modified) = metadata.modified().ok() else {
            continue;
        };
        if failures.contains(&video, size, modified) {
            continue;
        }
        let modified_ns = match crate::timestamp::time_ns(modified) {
            Some(nanos) => nanos,
            None => continue,
        };
        match prober(&video) {
            Ok(duration_ms) => {
                let size = i64::try_from(size).unwrap_or(i64::MAX);
                let _ = index.set_duration_ms(&video, size, modified_ns, duration_ms);
            }
            Err(message) => {
                failures.insert(&video, size, modified);
                on_failure(&video, &message);
            }
        }
    }
}

pub(crate) fn command_error(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("decoder exited with {}", output.status)
    } else {
        stderr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    #[test]
    fn background_scanning_finds_supported_videos_recursively() {
        let root = tempdir().unwrap();
        let clip = root.path().join("clip.mp4");
        let empty = root.path().join("empty.mp4");
        let notes = root.path().join("notes.txt");
        let nested = root.path().join("nested");
        let movie = nested.join("movie.MKV");
        std::fs::write(&clip, b"video").unwrap();
        std::fs::write(&empty, b"").unwrap();
        std::fs::write(&notes, b"text").unwrap();
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(&movie, b"video").unwrap();

        let mut found = video_paths(root.path());
        found.sort();
        assert_eq!(found, vec![clip, movie]);
    }

    fn scan_folder(database: &std::path::Path, root: &std::path::Path) {
        let mut index = crate::owned_index::OwnedIndex::open(database).unwrap();
        index.insert_root("root", root).unwrap();
        index.scan_root("root", root, |_| {}).unwrap();
    }

    #[test]
    fn enrichment_probes_only_videos_missing_durations() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("one.mp4"), b"video one").unwrap();
        std::fs::write(root.join("two.mp4"), b"video two").unwrap();
        std::fs::write(root.join("still.jpg"), b"image").unwrap();
        let database = directory.path().join("index.sqlite3");
        scan_folder(&database, &root);
        // One clip already probed: the pass must not probe it again.
        {
            let index = crate::owned_index::OwnedIndex::open(&database).unwrap();
            let metadata = std::fs::metadata(root.join("one.mp4")).unwrap();
            index
                .set_duration_ms(
                    &root.join("one.mp4"),
                    metadata.len() as i64,
                    crate::timestamp::time_ns(metadata.modified().unwrap()).unwrap(),
                    61_500,
                )
                .unwrap();
        }
        let probes = AtomicUsize::new(0);
        let failures = ProbeFailures::default();
        let mut failed = Vec::new();

        enrich_folder_durations(
            &database,
            &root,
            &failures,
            |video| {
                probes.fetch_add(1, Ordering::Relaxed);
                assert_eq!(video, root.join("two.mp4"));
                Ok(3_000)
            },
            |video, message| failed.push((video.to_path_buf(), message.to_owned())),
        );

        assert_eq!(probes.load(Ordering::Relaxed), 1);
        assert!(failed.is_empty());
        let index = crate::owned_index::OwnedIndex::open(&database).unwrap();
        let durations = index
            .durations_for(&[root.join("one.mp4"), root.join("two.mp4")])
            .unwrap();
        assert_eq!(durations[&root.join("one.mp4")], 61_500);
        assert_eq!(durations[&root.join("two.mp4")], 3_000);
    }

    #[test]
    fn enrichment_failures_report_once_and_never_overwrite() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("broken.mp4"), b"not a video").unwrap();
        let database = directory.path().join("index.sqlite3");
        scan_folder(&database, &root);
        let failures = ProbeFailures::default();
        let probes = AtomicUsize::new(0);
        let mut failed = Vec::new();
        let run = |failed: &mut Vec<(PathBuf, String)>| {
            enrich_folder_durations(
                &database,
                &root,
                &failures,
                |_| {
                    probes.fetch_add(1, Ordering::Relaxed);
                    Err::<u64, String>("ffprobe says no".into())
                },
                |video, message| failed.push((video.to_path_buf(), message.to_owned())),
            );
        };

        run(&mut failed);
        run(&mut failed);

        // Probed once, reported once: the second pass remembers.
        assert_eq!(probes.load(Ordering::Relaxed), 1);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, root.join("broken.mp4"));
        let index = crate::owned_index::OwnedIndex::open(&database).unwrap();
        assert!(index
            .durations_for(&[root.join("broken.mp4")])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn enrichment_loses_to_a_file_changed_mid_probe() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("clip.mp4"), b"short").unwrap();
        let database = directory.path().join("index.sqlite3");
        scan_folder(&database, &root);
        let failures = ProbeFailures::default();

        enrich_folder_durations(
            &database,
            &root,
            &failures,
            |video| {
                // The file grows while the probe runs, and the watcher
                // rescan commits the new size before the probe answers: the
                // duration read is for bytes that no longer exist.
                std::fs::write(video, b"a much longer clip").unwrap();
                let mut index = crate::owned_index::OwnedIndex::open(&database).unwrap();
                index.scan_root("root", &root, |_| {}).unwrap();
                Ok(9_000)
            },
            |_, _| panic!("a lost write is not a failure"),
        );

        // The rescan will re-queue it; the stale length never lands.
        let index = crate::owned_index::OwnedIndex::open(&database).unwrap();
        assert!(index
            .durations_for(&[root.join("clip.mp4")])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn parses_ffprobe_durations() {
        assert_eq!(parse_duration_seconds("61.500000"), Ok(61.5));
        assert_eq!(parse_duration_seconds("0.04"), Ok(0.04));
        assert_eq!(
            parse_duration_seconds("N/A"),
            Err("ffprobe returned an invalid duration: invalid float literal".into())
        );
        assert_eq!(
            parse_duration_seconds(""),
            Err(
                "ffprobe returned an invalid duration: cannot parse float from empty string".into()
            )
        );
    }

    #[test]
    fn rejects_missing_and_non_positive_durations() {
        for text in ["0", "0.0", "-3.5", "nan", "inf", "-inf"] {
            assert_eq!(
                parse_duration_seconds(text),
                Err("ffprobe returned a non-positive duration".into()),
                "{text:?} must not read as a length"
            );
        }
    }

    #[test]
    fn whole_milliseconds_round_from_seconds() {
        assert_eq!(seconds_to_ms(61.5), 61_500);
        assert_eq!(seconds_to_ms(0.0004), 0);
        assert_eq!(seconds_to_ms(0.0005), 1);
    }
}
