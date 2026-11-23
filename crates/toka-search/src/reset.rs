//! `toka reset`: put Toka back to a first-run state by removing everything it
//! has stored — search folders, the index and its caches, other caches, logs.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// One reset target: a path plus whether it may be removed recursively.
/// Targets naming a file Toka owns (index artifacts, logs) are file-only:
/// a directory sitting at such a path is foreign content — a directory
/// named `search-renames.json` holding media, say — and must never be
/// deleted. Only targets explicitly declared directories are removed
/// wholesale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetTarget {
    pub path: PathBuf,
    pub recursive: bool,
}

impl ResetTarget {
    /// A file Toka owns: removed when it is a file, refused with a clear
    /// error when a directory occupies its path.
    pub fn file(path: PathBuf) -> Self {
        Self {
            path,
            recursive: false,
        }
    }

    /// A directory Toka owns: removed wholesale, like a reset always has.
    pub fn dir(path: PathBuf) -> Self {
        Self {
            path,
            recursive: true,
        }
    }
}

/// What a reset removes, worked out before anything is touched so the same
/// list can be shown to the user and then acted on.
pub struct ResetPlan {
    /// Files and folders to delete. Ones that do not exist are skipped.
    pub targets: Vec<ResetTarget>,
    /// Stops the background indexer first, so it cannot rewrite what is being
    /// deleted. `None` where there is no such process.
    pub stop_indexer: Option<fn() -> Result<(), String>>,
}

impl ResetPlan {
    pub fn new(targets: Vec<ResetTarget>) -> Self {
        let mut unique: Vec<ResetTarget> = Vec::new();
        for target in targets {
            if !unique.iter().any(|known| known.path == target.path) {
                unique.push(target);
            }
        }
        // A path inside another listed path goes with its parent; listing it
        // separately would report a removal that already happened.
        let targets = unique
            .iter()
            .filter(|target| {
                !unique
                    .iter()
                    .any(|other| other.path != target.path && target.path.starts_with(&other.path))
            })
            .cloned()
            .collect();
        Self {
            targets,
            stop_indexer: None,
        }
    }
}

pub struct ResetReport {
    pub removed: Vec<PathBuf>,
    pub failures: Vec<String>,
}

pub fn apply(plan: &ResetPlan) -> ResetReport {
    let mut report = ResetReport {
        removed: Vec::new(),
        failures: Vec::new(),
    };
    // Fail closed: a supervisor that survives the stop would rewrite what
    // is being deleted — and, worse, deleting the lock file out from under
    // it silently breaks mutual exclusion (see `try_acquire_daemon_lock`).
    // When the stop fails, nothing is removed.
    if let Some(stop) = plan.stop_indexer {
        if let Err(message) = stop() {
            report.failures.push(message);
            return report;
        }
    }
    // Fail closed like the stop above: when a directory occupies a
    // file-only target's path, nothing is removed at all.
    for target in &plan.targets {
        if let Some(refusal) = refuse_unless_removable(target) {
            report.failures.push(refusal);
            return report;
        }
    }
    for target in &plan.targets {
        match remove(target) {
            Ok(true) => report.removed.push(target.path.clone()),
            Ok(false) => {}
            Err(error) => report.failures.push(error),
        }
    }
    report
}

/// Refuses a file-only target occupied by a directory: foreign content
/// that a recursive removal must never touch. `None` means removable —
/// anything but a real directory, since the removal below never follows
/// symlinks.
fn refuse_unless_removable(target: &ResetTarget) -> Option<String> {
    if target.recursive {
        return None;
    }
    match fs::symlink_metadata(&target.path) {
        Ok(metadata) if metadata.is_dir() => Some(refusal(&target.path)),
        _ => None,
    }
}

fn refusal(path: &Path) -> String {
    format!(
        "refusing to remove {}: a directory occupies this file's path",
        path.display()
    )
}

fn remove(target: &ResetTarget) -> Result<bool, String> {
    // Re-checked at removal time, so a directory swapped in after the
    // pre-check still refuses instead of deleting.
    if let Some(refusal) = refuse_unless_removable(target) {
        return Err(refusal);
    }
    // The operation follows the target KIND, never the observed metadata:
    // a file-only target always removes with `remove_file`, so a directory
    // swapped in after the check above fails cleanly instead of being
    // deleted recursively. Only declared-directory targets may remove
    // recursively — and they still dispatch on metadata, so a file at a
    // directory target's path removes as a file.
    if !target.recursive {
        return match fs::remove_file(&target.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(_) if is_dir_now(&target.path) => Err(refusal(&target.path)),
            Err(error) => Err(format!(
                "could not remove {}: {error}",
                target.path.display()
            )),
        };
    }
    let metadata = match fs::symlink_metadata(&target.path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "could not remove {}: {error}",
                target.path.display()
            ));
        }
    };
    if metadata.is_dir() {
        fs::remove_dir_all(&target.path)
            .map_err(|error| format!("could not remove {}: {error}", target.path.display()))?;
    } else {
        fs::remove_file(&target.path)
            .map_err(|error| format!("could not remove {}: {error}", target.path.display()))?;
    }
    Ok(true)
}

/// Whether `path` reads as a directory right now: message selection only,
/// after a file-only removal already failed — the failure itself is what
/// keeps the directory intact.
fn is_dir_now(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

#[cfg(test)]
mod tests {
    use super::{apply, remove, ResetPlan, ResetTarget};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn apply_removes_files_and_folders_and_skips_missing_ones() {
        let root = tempdir().unwrap();
        let folder = root.path().join("state");
        fs::create_dir_all(folder.join("indexes")).unwrap();
        fs::write(folder.join("indexes/a.db"), b"db").unwrap();
        let file = root.path().join("search.log");
        fs::write(&file, b"log").unwrap();
        let missing = root.path().join("never-existed");
        let kept = root.path().join("keep.txt");
        fs::write(&kept, b"mine").unwrap();

        let plan = ResetPlan::new(vec![
            ResetTarget::dir(folder.clone()),
            ResetTarget::file(file.clone()),
            ResetTarget::file(missing),
        ]);
        let report = apply(&plan);

        assert!(report.failures.is_empty());
        assert_eq!(report.removed, vec![folder.clone(), file.clone()]);
        assert!(!folder.exists());
        assert!(!file.exists());
        assert!(kept.exists());
    }

    #[test]
    fn plan_drops_duplicates_and_paths_inside_other_listed_paths() {
        let plan = ResetPlan::new(vec![
            ResetTarget::dir("/a/toka".into()),
            ResetTarget::dir("/a/toka/indexes".into()),
            ResetTarget::dir("/a/toka".into()),
            ResetTarget::dir("/a/tokaish".into()),
        ]);

        assert_eq!(
            plan.targets,
            vec![
                ResetTarget::dir("/a/toka".into()),
                ResetTarget::dir("/a/tokaish".into()),
            ]
        );
    }

    #[test]
    fn a_failed_stop_is_reported_and_nothing_is_removed() {
        let root = tempdir().unwrap();
        let file = root.path().join("index-status.json");
        fs::write(&file, b"{}").unwrap();
        let mut plan = ResetPlan::new(vec![ResetTarget::file(file.clone())]);
        plan.stop_indexer = Some(|| Err("systemctl failed".into()));

        let report = apply(&plan);

        assert_eq!(report.failures, vec!["systemctl failed".to_owned()]);
        assert!(report.removed.is_empty());
        assert!(file.exists(), "a failed stop must remove nothing");
    }

    #[test]
    fn a_successful_stop_proceeds_with_the_removal() {
        let root = tempdir().unwrap();
        let file = root.path().join("index-status.json");
        fs::write(&file, b"{}").unwrap();
        let mut plan = ResetPlan::new(vec![ResetTarget::file(file.clone())]);
        plan.stop_indexer = Some(|| Ok(()));

        let report = apply(&plan);

        assert!(report.failures.is_empty());
        assert_eq!(report.removed, vec![file.clone()]);
        assert!(!file.exists());
    }

    #[test]
    fn a_directory_at_a_file_target_refuses_and_removes_nothing() {
        // A directory named like an index artifact, holding media: the
        // reset refuses it with a clear error and removes nothing else
        // either — not even the legitimate sibling file.
        let root = tempdir().unwrap();
        let artifact = root.path().join("search-renames.json");
        fs::create_dir(&artifact).unwrap();
        let media = artifact.join("clip.mp4");
        fs::write(&media, b"video").unwrap();
        let sibling = root.path().join("index.sqlite3");
        fs::write(&sibling, b"db").unwrap();

        let plan = ResetPlan::new(vec![
            ResetTarget::file(sibling.clone()),
            ResetTarget::file(artifact.clone()),
        ]);
        let report = apply(&plan);

        assert_eq!(report.failures.len(), 1);
        assert!(
            report.failures[0].contains("refusing to remove")
                && report.failures[0].contains("search-renames.json"),
            "{}",
            report.failures.join("\n")
        );
        assert!(report.removed.is_empty());
        assert!(media.is_file(), "media inside survives");
        assert!(sibling.is_file(), "the sibling survives too");
    }

    #[test]
    fn removal_of_a_file_target_never_deletes_a_directory() {
        // Straight at `remove`, past the pre-check: a directory sitting at
        // a file-only artifact's path — as a swap after the check would
        // leave it — refuses with its contents intact, because file-only
        // targets always remove with `remove_file`.
        let root = tempdir().unwrap();
        let artifact = root.path().join("index.sqlite3");
        fs::create_dir(&artifact).unwrap();
        let media = artifact.join("clip.mp4");
        fs::write(&media, b"video").unwrap();

        let error = remove(&ResetTarget::file(artifact.clone())).unwrap_err();

        assert!(
            error.contains("refusing to remove") && error.contains("index.sqlite3"),
            "{error}"
        );
        assert!(media.is_file(), "contents survive the refusal");
        assert!(artifact.is_dir(), "the directory itself survives");
    }

    #[test]
    fn removal_of_a_file_target_unlinks_without_following_symlinks() {
        // A symlink at a file-only target's path unlinks the link itself —
        // `remove_file` never follows it into the target.
        let root = tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("clip.mp4"), b"video").unwrap();
        let link = root.path().join("search.log");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(not(unix))]
        std::os::windows::fs::symlink_dir(&target, &link).unwrap();

        assert!(remove(&ResetTarget::file(link.clone())).unwrap());

        assert!(!link.exists() && fs::symlink_metadata(&link).is_err());
        assert!(target.join("clip.mp4").is_file());
    }
}
