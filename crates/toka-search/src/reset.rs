//! `toka reset`: put Toka back to a first-run state by removing everything it
//! has stored — search folders, the index and its caches, other caches, logs.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// What a reset removes, worked out before anything is touched so the same
/// list can be shown to the user and then acted on.
pub struct ResetPlan {
    /// Files and folders to delete. Ones that do not exist are skipped.
    pub paths: Vec<PathBuf>,
    /// Stops the background indexer first, so it cannot rewrite what is being
    /// deleted. `None` where there is no such process.
    pub stop_indexer: Option<fn() -> Result<(), String>>,
}

impl ResetPlan {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        let mut unique = Vec::new();
        for path in paths {
            if !unique.contains(&path) {
                unique.push(path);
            }
        }
        // A path inside another listed path goes with its parent; listing it
        // separately would report a removal that already happened.
        let paths = unique
            .iter()
            .filter(|path| {
                !unique
                    .iter()
                    .any(|other| other != *path && path.starts_with(other))
            })
            .cloned()
            .collect();
        Self {
            paths,
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
    for path in &plan.paths {
        match remove(path) {
            Ok(true) => report.removed.push(path.clone()),
            Ok(false) => {}
            Err(error) => report
                .failures
                .push(format!("could not remove {}: {error}", path.display())),
        }
    }
    report
}

fn remove(path: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{apply, ResetPlan};
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

        let plan = ResetPlan::new(vec![folder.clone(), file.clone(), missing]);
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
            "/a/toka".into(),
            "/a/toka/indexes".into(),
            "/a/toka".into(),
            "/a/tokaish".into(),
        ]);

        assert_eq!(
            plan.paths,
            vec![
                std::path::PathBuf::from("/a/toka"),
                std::path::PathBuf::from("/a/tokaish")
            ]
        );
    }

    #[test]
    fn a_failed_stop_is_reported_and_nothing_is_removed() {
        let root = tempdir().unwrap();
        let file = root.path().join("index-status.json");
        fs::write(&file, b"{}").unwrap();
        let mut plan = ResetPlan::new(vec![file.clone()]);
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
        let mut plan = ResetPlan::new(vec![file.clone()]);
        plan.stop_indexer = Some(|| Ok(()));

        let report = apply(&plan);

        assert!(report.failures.is_empty());
        assert_eq!(report.removed, vec![file.clone()]);
        assert!(!file.exists());
    }
}
