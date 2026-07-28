//! Filename tagging, ported from the standalone `tag` library.
//!
//! Tags live in the name itself, inside square brackets before the extension:
//! `my_home_video [cute home].mp4`. Directories are tagged the same way, with
//! no extension. Tags are lowercased, sorted, and never duplicated.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct TagUpdate {
    pub path: PathBuf,
    pub tags: Vec<String>,
}

/// A normalized tag set: split on whitespace, lowercased, deduplicated, sorted.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tags(Vec<String>);

impl Tags {
    pub fn new<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut values: Vec<String> = values
            .into_iter()
            .flat_map(|value| {
                value
                    .as_ref()
                    .split_whitespace()
                    .map(str::to_lowercase)
                    .collect::<Vec<_>>()
            })
            .collect();
        values.sort();
        values.dedup();
        Self(values)
    }

    pub fn parse_file_name(file_name: &str) -> Self {
        let (stem, _) = split_extension(file_name);
        Self::new(tag_block(stem))
    }

    pub fn parse_dir_name(dir_name: &str) -> Self {
        Self::new(tag_block(dir_name))
    }

    pub fn values(&self) -> &[String] {
        &self.0
    }

    pub fn into_values(self) -> Vec<String> {
        self.0
    }

    pub fn merge(&self, other: &Tags) -> Tags {
        Tags::new(self.0.iter().chain(other.0.iter()))
    }

    pub fn remove(&self, other: &Tags) -> Tags {
        Tags::new(self.0.iter().filter(|tag| !other.0.contains(tag)))
    }

    /// Rewrites the tag block of a file name, keeping the extension in place.
    pub fn apply_to_file(&self, file_name: &str) -> String {
        let (stem, extension) = split_extension(file_name);
        format!("{}{extension}", self.apply_to_base(stem))
    }

    /// Rewrites the tag block of a directory name.
    pub fn apply_to_dir(&self, dir_name: &str) -> String {
        self.apply_to_base(dir_name)
    }

    fn apply_to_base(&self, name: &str) -> String {
        let base = strip_tag_block(name);
        if self.0.is_empty() {
            base.to_owned()
        } else {
            format!("{base} [{}]", self.0.join(" "))
        }
    }
}

/// Splits a file name into stem and extension. A leading dot does not start an
/// extension, so dotfiles keep their whole name as the stem.
fn split_extension(file_name: &str) -> (&str, &str) {
    match file_name.rfind('.') {
        Some(index) if index > 0 => file_name.split_at(index),
        _ => (file_name, ""),
    }
}

/// The contents of the bracketed tag block ending `name`, if it has one.
fn tag_block(name: &str) -> Option<&str> {
    let without_close = name.strip_suffix(']')?;
    let open = without_close.rfind('[')?;
    let block = &without_close[open + 1..];
    (!block.is_empty() && !block.contains(']')).then_some(block)
}

/// `name` without its tag block, and without the whitespace preceding it.
fn strip_tag_block(name: &str) -> &str {
    match tag_block(name) {
        Some(block) => name[..name.len() - block.len() - 2].trim_end(),
        None => name,
    }
}

/// Current tags of a file or directory, read from its name.
pub fn get(path: &Path) -> Vec<String> {
    parse(path).into_values()
}

/// Replaces the whole tag set; an empty list clears every tag. `path` may omit
/// an existing name's trailing tag block, provided it names exactly one entry.
pub fn set(path: &Path, tags: &[String]) -> Result<TagUpdate, String> {
    let path = resolve(path, true)?;
    rename(&path, &validate(tags, true)?)
}

/// Adds tags to those a file or directory already carries.
pub fn add(path: &Path, tags: &[String]) -> Result<TagUpdate, String> {
    let path = resolve(path, false)?;
    let added = validate(tags, false)?;
    rename(&path, &parse(&path).merge(&added))
}

/// Removes tags from those a file or directory already carries.
pub fn remove(path: &Path, tags: &[String]) -> Result<TagUpdate, String> {
    let path = resolve(path, false)?;
    let removed = validate(tags, false)?;
    rename(&path, &parse(&path).remove(&removed))
}

fn parse(path: &Path) -> Tags {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if path.is_dir() {
        Tags::parse_dir_name(&name)
    } else {
        Tags::parse_file_name(&name)
    }
}

/// Tags must be non-empty single words without brackets, so that they survive a
/// round trip through a name's tag block.
fn validate(tags: &[String], allow_empty: bool) -> Result<Tags, String> {
    if tags.is_empty() && !allow_empty {
        return Err("At least one tag is required.".to_owned());
    }
    let invalid = tags.iter().find(|tag| {
        tag.is_empty() || tag.contains(['[', ']']) || tag.chars().any(char::is_whitespace)
    });
    match invalid {
        Some(tag) => Err(format!(
            "Tags must be single words without brackets: {tag:?}"
        )),
        None => Ok(Tags::new(tags)),
    }
}

fn resolve(path: &Path, untagged: bool) -> Result<PathBuf, String> {
    let path = absolute(path);
    if exists(&path) {
        return Ok(path);
    }
    if untagged {
        let mut matches = untagged_matches(&path);
        match matches.len() {
            1 => return Ok(matches.remove(0)),
            0 => {}
            _ => return Err(format!("That path is ambiguous: {}", path.display())),
        }
    }
    Err(format!("That path does not exist: {}", path.display()))
}

/// Entries in `path`'s parent whose name, with its tag block removed, is
/// `path`'s name — the tagged entries `path` could be referring to.
fn untagged_matches(path: &Path) -> Vec<PathBuf> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            let candidate = entry.file_name().to_string_lossy().into_owned();
            let untagged = if entry.path().is_dir() {
                Tags::default().apply_to_dir(&candidate)
            } else {
                Tags::default().apply_to_file(&candidate)
            };
            untagged.as_str() == name
        })
        .map(|entry| entry.path())
        .collect()
}

fn rename(path: &Path, tags: &Tags) -> Result<TagUpdate, String> {
    let target = target_path(path, tags);
    if target != path {
        if exists(&target) {
            return Err(format!("That name already exists: {}", target.display()));
        }
        fs::rename(path, &target)
            .map_err(|error| format!("{} could not be renamed: {error}", path.display()))?;
    }
    Ok(TagUpdate {
        tags: tags.values().to_vec(),
        path: target,
    })
}

fn target_path(path: &Path, tags: &Tags) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let renamed = if path.is_dir() {
        tags.apply_to_dir(&name)
    } else {
        tags.apply_to_file(&name)
    };
    path.with_file_name(renamed)
}

/// Symlinks count even when they dangle, so tagging never clobbers one.
fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;
    use tempfile::TempDir;

    fn tags(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn file(directory: &TempDir, name: &str) -> PathBuf {
        let path = directory.path().join(name);
        fs::write(&path, b"video").unwrap();
        path
    }

    fn name_of(path: &Path) -> String {
        path.file_name().unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn parses_tags_from_a_file_name() {
        assert_eq!(
            Tags::parse_file_name("clip [summer vacation].mp4").values(),
            ["summer", "vacation"]
        );
    }

    #[test]
    fn parses_tags_from_a_directory_name() {
        assert_eq!(
            Tags::parse_dir_name("Trips [europe vacations]").values(),
            ["europe", "vacations"]
        );
    }

    #[test]
    fn parses_no_tags_without_a_tag_block() {
        assert!(Tags::parse_file_name("clip.mp4").values().is_empty());
        assert!(Tags::parse_file_name("clip [].mp4").values().is_empty());
        assert!(Tags::parse_dir_name("Trips").values().is_empty());
    }

    #[test]
    fn parses_a_tag_block_with_no_leading_space() {
        assert_eq!(
            Tags::parse_file_name("clip[summer].mp4").values(),
            ["summer"]
        );
    }

    #[test]
    fn parses_tags_only_from_the_end_of_a_name() {
        let tags = Tags::parse_file_name("clip [home] trailing.mp4");
        assert!(tags.values().is_empty());
    }

    #[test]
    fn normalizes_case_whitespace_and_duplicates() {
        assert_eq!(
            Tags::new(["Vacation", "home vacation", "HOME"]).values(),
            ["home", "vacation"]
        );
    }

    #[test]
    fn merges_without_duplicating_and_stays_sorted() {
        let merged = Tags::new(["sunny", "vacations"]).merge(&Tags::new(["beach", "vacations"]));
        assert_eq!(merged.values(), ["beach", "sunny", "vacations"]);
    }

    #[test]
    fn removes_tags_regardless_of_case() {
        let remaining = Tags::new(["beach", "home"]).remove(&Tags::new(["BEACH"]));
        assert_eq!(remaining.values(), ["home"]);
    }

    #[test]
    fn applies_tags_to_a_file_name() {
        assert_eq!(
            Tags::new(["summer"]).apply_to_file("clip.mp4"),
            "clip [summer].mp4"
        );
        assert_eq!(
            Tags::new(["summer", "vacation"]).apply_to_file("clip [summer].mp4"),
            "clip [summer vacation].mp4"
        );
        assert_eq!(
            Tags::new(["summer"]).apply_to_file("clip[summer].mp4"),
            "clip [summer].mp4"
        );
        assert_eq!(
            Tags::default().apply_to_file("clip [summer].mp4"),
            "clip.mp4"
        );
    }

    #[test]
    fn applies_tags_to_a_directory_name() {
        assert_eq!(
            Tags::new(["summer"]).apply_to_dir("Trips"),
            "Trips [summer]"
        );
        assert_eq!(
            Tags::new(["europe", "summer"]).apply_to_dir("Trips [summer]"),
            "Trips [europe summer]"
        );
        assert_eq!(
            Tags::new(["summer"]).apply_to_dir("Trips[summer]"),
            "Trips [summer]"
        );
        assert_eq!(Tags::default().apply_to_dir("Trips [summer]"), "Trips");
    }

    #[test]
    fn get_reads_tags_from_the_name() {
        let directory = tempdir().unwrap();
        let video = file(&directory, "Clip [beach home].mp4");
        assert_eq!(get(&video), ["beach", "home"]);
        assert!(get(&directory.path().join("missing.mp4")).is_empty());
    }

    #[test]
    fn add_writes_canonical_lowercase_sorted_names() {
        let directory = tempdir().unwrap();
        let video = file(&directory, "Clip[HOME].mp4");

        let update = add(&video, &tags(&["Vacation", "home"])).unwrap();

        assert_eq!(name_of(&update.path), "Clip [home vacation].mp4");
        assert_eq!(update.tags, ["home", "vacation"]);
        assert!(update.path.exists());
        assert!(!video.exists());
    }

    #[test]
    fn add_is_a_no_op_when_the_tag_is_already_present() {
        let directory = tempdir().unwrap();
        let video = file(&directory, "Clip [home].mp4");

        let update = add(&video, &tags(&["HOME"])).unwrap();

        assert_eq!(update.path, video);
        assert_eq!(update.tags, ["home"]);
    }

    #[test]
    fn set_replaces_and_clears_the_whole_tag_set() {
        let directory = tempdir().unwrap();
        let video = file(&directory, "Clip [home].mp4");

        let update = set(&video, &tags(&["Beach", "home"])).unwrap();
        assert_eq!(name_of(&update.path), "Clip [beach home].mp4");

        let cleared = set(&update.path, &[]).unwrap();
        assert_eq!(name_of(&cleared.path), "Clip.mp4");
        assert!(cleared.tags.is_empty());
        assert!(cleared.path.exists());
    }

    #[test]
    fn set_accepts_the_untagged_path_of_a_tagged_file() {
        let directory = tempdir().unwrap();
        file(&directory, "Clip [home].mp4");

        let update = set(&directory.path().join("Clip.mp4"), &tags(&["Beach"])).unwrap();

        assert_eq!(name_of(&update.path), "Clip [beach].mp4");
        assert!(update.path.exists());
    }

    #[test]
    fn set_rejects_an_untagged_path_matching_several_files() {
        let directory = tempdir().unwrap();
        file(&directory, "Clip [home].mp4");
        file(&directory, "Clip [vacation].mp4");

        let error = set(&directory.path().join("Clip.mp4"), &tags(&["Beach"])).unwrap_err();

        assert!(error.contains("ambiguous"), "{error}");
    }

    #[test]
    fn remove_reports_the_remaining_tags() {
        let directory = tempdir().unwrap();
        let video = file(&directory, "Clip [beach home].mp4");

        let update = remove(&video, &tags(&["BEACH"])).unwrap();

        assert_eq!(name_of(&update.path), "Clip [home].mp4");
        assert_eq!(update.tags, ["home"]);
        assert_eq!(get(&update.path), ["home"]);
    }

    #[test]
    fn remove_clears_the_tag_block_with_the_last_tag() {
        let directory = tempdir().unwrap();
        let video = file(&directory, "Clip [home].mp4");

        let update = remove(&video, &tags(&["home"])).unwrap();

        assert_eq!(name_of(&update.path), "Clip.mp4");
        assert!(update.tags.is_empty());
    }

    #[test]
    fn tags_a_directory() {
        let directory = tempdir().unwrap();
        let trips = directory.path().join("Trips");
        fs::create_dir(&trips).unwrap();

        let update = add(&trips, &tags(&["Vacations"])).unwrap();

        assert_eq!(name_of(&update.path), "Trips [vacations]");
        assert!(update.path.is_dir());
    }

    #[test]
    fn refuses_to_overwrite_an_existing_name() {
        let directory = tempdir().unwrap();
        let video = file(&directory, "Clip.mp4");
        let occupied = file(&directory, "Clip [home].mp4");

        let error = add(&video, &tags(&["home"])).unwrap_err();

        assert!(error.contains("already exists"), "{error}");
        assert!(video.exists());
        assert!(occupied.exists());
    }

    #[test]
    fn rejects_paths_that_do_not_exist() {
        let directory = tempdir().unwrap();
        let missing = directory.path().join("missing.mp4");

        assert!(add(&missing, &tags(&["home"]))
            .unwrap_err()
            .contains("does not exist"));
        assert!(set(&missing, &tags(&["home"]))
            .unwrap_err()
            .contains("does not exist"));
    }

    #[test]
    fn rejects_tags_that_cannot_round_trip_through_a_name() {
        let directory = tempdir().unwrap();
        let video = file(&directory, "Clip.mp4");

        for invalid in [" ", "", "home vacation", "ho[me]"] {
            let error = add(&video, &tags(&[invalid])).unwrap_err();
            assert!(error.contains("single words"), "{invalid:?}: {error}");
        }
        assert!(add(&video, &[]).unwrap_err().contains("At least one tag"));
        assert_eq!(name_of(&video), "Clip.mp4");
    }
}
