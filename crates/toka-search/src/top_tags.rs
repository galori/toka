//! The most used tags across the index, for one-gesture tagging in the app.
//!
//! The indexer daemon recounts every few minutes and writes the result to a
//! small JSON file beside the index database, so the app reads a short list
//! instead of scanning every file. A sidecar file rather than a table keeps
//! the index schema as it is: an older Toka can still open the database.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::owned_index::OwnedIndex;

/// Tags the picker offers.
pub const TOP_TAG_LIMIT: usize = 10;

/// The file name, beside the index database.
pub const TOP_TAGS_FILE_NAME: &str = "top-tags.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopTag {
    pub tag: String,
    pub count: u64,
}

/// Favorites have their own control, so they never take a slot.
fn is_favorite_tag(tag: &str) -> bool {
    tag == "fv"
        || (tag.len() == 3 && tag.starts_with("fv") && matches!(tag.as_bytes()[2], b'1'..=b'5'))
}

/// The most used tags, most used first and alphabetical among equals.
pub fn compute(index: &OwnedIndex, limit: usize) -> Result<Vec<TopTag>, String> {
    let counts = index.tag_counts().map_err(|error| error.to_string())?;
    let mut tags: Vec<TopTag> = counts
        .into_iter()
        .filter(|(tag, _)| !is_favorite_tag(tag))
        .map(|(tag, count)| TopTag { tag, count })
        .collect();
    tags.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tag.cmp(&b.tag)));
    tags.truncate(limit);
    Ok(tags)
}

pub fn path_for(database: &Path) -> PathBuf {
    database.with_file_name(TOP_TAGS_FILE_NAME)
}

/// Recounts and rewrites the sidecar. The write is atomic, so a reader never
/// sees half a file.
pub fn refresh(database: &Path) -> Result<Vec<TopTag>, String> {
    let index = OwnedIndex::open_read_only(database).map_err(|error| error.to_string())?;
    let tags = compute(&index, TOP_TAG_LIMIT)?;
    let target = path_for(database);
    // Unique per call: the daemon and the app may recount at the same moment.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temporary = target.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let json = serde_json::to_vec(&tags).map_err(|error| error.to_string())?;
    fs::write(&temporary, json).map_err(|error| error.to_string())?;
    if let Err(error) = fs::rename(&temporary, &target) {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    Ok(tags)
}

/// The last list the daemon wrote, or `None` before its first pass.
pub fn read(database: &Path) -> Option<Vec<TopTag>> {
    let bytes = fs::read(path_for(database)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// [`read`], but `None` when the file is older than `max_age`: the daemon may
/// have stopped, or the app may be running the index itself, and a frozen
/// list would hide every tag used since.
pub fn read_fresh(database: &Path, max_age: std::time::Duration) -> Option<Vec<TopTag>> {
    let modified = fs::metadata(path_for(database)).ok()?.modified().ok()?;
    if modified.elapsed().is_ok_and(|age| age > max_age) {
        return None;
    }
    read(database)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn indexed(names: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        let root = directory.path().join("media");
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        for name in names {
            fs::write(root.join(name), b"video").unwrap();
        }
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        (directory, database)
    }

    fn tags(top: &[TopTag]) -> Vec<(&str, u64)> {
        top.iter().map(|t| (t.tag.as_str(), t.count)).collect()
    }

    #[test]
    fn counts_files_per_tag_most_used_first_then_alphabetical() {
        let (_directory, database) = indexed(&[
            "a [beach sun].mp4",
            "b [beach].mp4",
            "c [cat].mp4",
            "d [beach cat].mp4",
            "untagged.mp4",
        ]);
        let index = OwnedIndex::open_read_only(&database).unwrap();

        let top = compute(&index, 10).unwrap();

        assert_eq!(tags(&top), [("beach", 3), ("cat", 2), ("sun", 1)]);
    }

    #[test]
    fn leaves_out_favorite_tags_and_honors_the_limit() {
        let (_directory, database) =
            indexed(&["a [fv fv5 x].mp4", "b [fv fv2 y].mp4", "c [fv z].mp4"]);
        let index = OwnedIndex::open_read_only(&database).unwrap();

        let top = compute(&index, 2).unwrap();

        assert_eq!(tags(&top), [("x", 1), ("y", 1)]);
    }

    #[test]
    fn read_fresh_ignores_a_list_older_than_the_limit() {
        let (_directory, database) = indexed(&["a [beach].mp4"]);
        refresh(&database).unwrap();

        assert!(read_fresh(&database, std::time::Duration::from_secs(60)).is_some());
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(read_fresh(&database, std::time::Duration::ZERO).is_none());
    }

    #[test]
    fn an_empty_index_has_no_top_tags() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        OwnedIndex::open(&database).unwrap();
        let index = OwnedIndex::open_read_only(&database).unwrap();

        assert!(compute(&index, 10).unwrap().is_empty());
    }

    #[test]
    fn refresh_writes_the_file_that_read_returns() {
        let (_directory, database) = indexed(&["a [beach].mp4", "b [beach].mp4"]);
        assert_eq!(read(&database), None);

        let written = refresh(&database).unwrap();

        assert_eq!(tags(&written), [("beach", 2)]);
        assert_eq!(read(&database), Some(written));
    }
}
