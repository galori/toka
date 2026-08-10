//! The playlist file Toka was launched with.
//!
//! Extended M3U is the format every player Toka hands a playlist to
//! understands, and reading one back is the same few lines as writing it: the
//! format carries no structure beyond one entry per line, so a playlist made
//! anywhere — by Toka's own handoff, by VLC, by hand — opens the same way.

use crate::search::{is_supported_media, is_supported_video, MediaType};
use std::{
    borrow::Cow,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// The extensions a playlist file goes by. `m3u8` is the UTF-8 one Toka writes;
/// `m3u` is the same format under its older name.
const PLAYLIST_EXTENSIONS: [&str; 2] = ["m3u8", "m3u"];

/// The argument Toka was launched with that names what to play: a playlist
/// file, a single video, or a folder whose videos become the playlist.
/// `arguments` is the whole command line, program name included, the way the
/// operating system hands it over — including when the viewer opened the item
/// in a file manager rather than typing the command themselves.
pub fn from_arguments(arguments: impl IntoIterator<Item = OsString>) -> Option<PathBuf> {
    arguments
        .into_iter()
        .skip(1)
        .map(PathBuf::from)
        .find(|path| is_playlist(path) || is_supported_video(path) || path.is_dir())
}

fn is_playlist(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_lowercase)
        .is_some_and(|extension| PLAYLIST_EXTENSIONS.contains(&extension.as_str()))
}

/// The entries an extended M3U playlist lists, in its order. A line starting
/// with `#` is a directive — `#EXTM3U`, `#EXTINF` and the rest — and a blank one
/// separates nothing from nothing; everything else names one entry.
///
/// An entry may be written relative to the playlist, so `directory` is the
/// folder the playlist file itself sits in.
pub fn parse(source: &str, directory: &Path) -> Vec<PathBuf> {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let entry = Path::new(line);
            if entry.is_absolute() {
                entry.to_path_buf()
            } else {
                directory.join(entry)
            }
        })
        .collect()
}

/// Every video the playlist file at `path` still points at, or why there are
/// none to play.
///
/// An entry that has since been deleted, and an entry that is not a video Toka
/// can play, are left out: a playlist is a list of what to play, and one stale
/// line in it is no reason to refuse the rest. A playlist Toka cannot read and
/// one whose entries have all gone are the same answer to the viewer, so both
/// come back as the reason rather than as an empty playlist to start.
pub fn videos(path: &Path) -> Result<Vec<PathBuf>, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("{} could not be read: {error}", name(path)))?;
    let directory = path.parent().unwrap_or_else(|| Path::new(""));
    let videos: Vec<PathBuf> = parse(&source, directory)
        .into_iter()
        .filter(|entry| entry.is_file() && is_supported_media(entry, MediaType::Both))
        .collect();
    if videos.is_empty() {
        return Err(format!("{} lists no videos Toka can play.", name(path)));
    }
    Ok(videos)
}

/// Every supported video under `directory` and its subfolders, recursively.
/// The walk skips files Toka cannot play and any entry that cannot be read.
pub fn collect_directory_videos(directory: &Path) -> Result<Vec<PathBuf>, String> {
    if !directory.is_dir() {
        return Err(format!(
            "{} could not be read: not a directory",
            name(directory)
        ));
    }
    let mut videos = Vec::new();
    collect_recursive(directory, &mut videos)
        .map_err(|error| format!("{} could not be read: {error}", name(directory)))?;
    if videos.is_empty() {
        return Err(format!(
            "{} contains no videos Toka can play.",
            name(directory)
        ));
    }
    Ok(videos)
}

fn collect_recursive(directory: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_recursive(&path, out)?;
        } else if path.is_file() && is_supported_video(&path) {
            out.push(path);
        }
    }
    Ok(())
}

/// Shuffles `paths` with a fresh unpredictable seed, like every search's
/// results are shuffled. A folder's videos should start in a different order
/// each time Toka is asked to play it.
pub fn shuffle_paths(paths: &mut [PathBuf]) {
    let seed = fresh_seed();
    shuffle(paths, seed);
}

fn fresh_seed() -> u64 {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or_default();
    let calls = CALLS.fetch_add(1, Ordering::Relaxed);
    let mut state = now ^ calls.wrapping_mul(GOLDEN_GAMMA);
    next_random(&mut state)
}

const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

fn next_random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(GOLDEN_GAMMA);
    let mut drawn = *state;
    drawn = (drawn ^ (drawn >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    drawn = (drawn ^ (drawn >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    drawn ^ (drawn >> 31)
}

fn shuffle(paths: &mut [PathBuf], seed: u64) {
    let mut state = seed;
    for index in (1..paths.len()).rev() {
        let swap = (next_random(&mut state) % (index as u64 + 1)) as usize;
        paths.swap(index, swap);
    }
}

/// What to call the playlist when telling the viewer about it: the file's own
/// name, which is what they opened, rather than the whole path to it.
pub fn name(path: &Path) -> Cow<'_, str> {
    let name = path.file_name().unwrap_or(path.as_os_str());
    name.to_string_lossy()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn arguments(arguments: &[&str]) -> Vec<OsString> {
        arguments.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_playlist_toka_was_launched_with_is_the_argument_naming_one() {
        assert_eq!(
            from_arguments(arguments(&["toka", "/Videos/summer.m3u8"])),
            Some(PathBuf::from("/Videos/summer.m3u8"))
        );
        assert_eq!(
            from_arguments(arguments(&["toka", "--flag", "playlists/Old.M3U"])),
            Some(PathBuf::from("playlists/Old.M3U"))
        );
    }

    #[test]
    fn launching_toka_without_a_playlist_names_none() {
        assert_eq!(from_arguments(arguments(&["toka"])), None);
        // A bare flag or unsupported file carries no launch — a playlist is not
        // assumed. Video files are handled separately via is_supported_video.
        assert_eq!(
            from_arguments(arguments(&["toka", "--flag", "notes.txt"])),
            None
        );
        // The program itself is not the playlist, whatever it is called.
        assert_eq!(from_arguments(arguments(&["/usr/bin/toka.m3u8"])), None);
    }

    #[test]
    fn launching_toka_with_a_single_video_is_recognised() {
        assert_eq!(
            from_arguments(arguments(&["toka", "/Videos/clip.mp4"])),
            Some(PathBuf::from("/Videos/clip.mp4"))
        );
        assert_eq!(
            from_arguments(arguments(&["toka", "--flag", "movie.MKV"])),
            Some(PathBuf::from("movie.MKV"))
        );
        // Unsupported extensions are not launch targets.
        assert_eq!(
            from_arguments(arguments(&["toka", "/Videos/notes.txt"])),
            None
        );
    }

    #[test]
    fn launching_toka_with_a_folder_is_recognised() {
        let directory = tempdir().unwrap();
        let arg = directory.path().to_string_lossy().into_owned();
        assert_eq!(
            from_arguments(arguments(&["toka", &arg])),
            Some(directory.path().to_path_buf())
        );
    }

    #[test]
    fn launching_prefers_the_first_launchable_argument() {
        assert_eq!(
            from_arguments(arguments(&[
                "toka",
                "--flag",
                "/Videos/a.mp4",
                "/Videos/b.m3u8"
            ])),
            Some(PathBuf::from("/Videos/a.mp4"))
        );
        let directory = tempdir().unwrap();
        let dir_arg = directory.path().to_string_lossy().into_owned();
        assert_eq!(
            from_arguments(arguments(&["toka", &dir_arg, "/Videos/a.mp4"])),
            Some(directory.path().to_path_buf())
        );
    }

    #[test]
    fn video_extension_matching_is_case_insensitive() {
        assert_eq!(
            from_arguments(arguments(&["toka", "clip.Mp4"])),
            Some(PathBuf::from("clip.Mp4"))
        );
        assert_eq!(
            from_arguments(arguments(&["toka", "clip.MOV"])),
            Some(PathBuf::from("clip.MOV"))
        );
        assert_eq!(
            from_arguments(arguments(&["toka", "clip.AVI"])),
            Some(PathBuf::from("clip.AVI"))
        );
    }

    #[test]
    fn the_entries_are_the_lines_that_are_neither_blank_nor_directives() {
        let source = "#EXTM3U\n\n#EXTINF:12,Beach day\n/Videos/beach.mp4\n/Videos/party.mkv\n";

        assert_eq!(
            parse(source, Path::new("/Playlists")),
            [
                PathBuf::from("/Videos/beach.mp4"),
                PathBuf::from("/Videos/party.mkv")
            ]
        );
    }

    #[test]
    fn an_entry_written_relative_to_the_playlist_is_resolved_against_it() {
        let source = "clips/beach.mp4\n../party.mkv\n/Videos/absolute.mp4\n";

        assert_eq!(
            parse(source, Path::new("/Playlists/Summer")),
            [
                PathBuf::from("/Playlists/Summer/clips/beach.mp4"),
                PathBuf::from("/Playlists/Summer/../party.mkv"),
                PathBuf::from("/Videos/absolute.mp4")
            ]
        );
    }

    #[test]
    fn a_windows_line_ending_is_not_part_of_the_path_it_ends() {
        let source = "#EXTM3U\r\n/Videos/beach.mp4\r\n  /Videos/party.mkv  \r\n";

        assert_eq!(
            parse(source, Path::new("/Playlists")),
            [
                PathBuf::from("/Videos/beach.mp4"),
                PathBuf::from("/Videos/party.mkv")
            ]
        );
    }

    #[test]
    fn a_playlist_of_nothing_but_directives_has_no_entries() {
        assert!(parse("#EXTM3U\n\n", Path::new("/Playlists")).is_empty());
        assert!(parse("", Path::new("/Playlists")).is_empty());
    }

    #[test]
    fn the_playlists_videos_are_the_entries_that_are_still_playable() {
        let directory = tempdir().unwrap();
        let beach = directory.path().join("beach.mp4");
        let party = directory.path().join("party.mkv");
        let notes = directory.path().join("notes.txt");
        for path in [&beach, &party, &notes] {
            fs::write(path, b"test").unwrap();
        }
        let playlist = directory.path().join("summer.m3u8");
        fs::write(
            &playlist,
            "#EXTM3U\nbeach.mp4\ngone.mp4\nnotes.txt\nhttps://example.com/stream.mp4\nparty.mkv\n",
        )
        .unwrap();

        assert_eq!(videos(&playlist).unwrap(), [beach, party]);
    }

    #[test]
    fn a_playlist_that_cannot_be_read_says_so_and_names_itself() {
        let directory = tempdir().unwrap();
        let missing = directory.path().join("gone.m3u8");

        let error = videos(&missing).unwrap_err();

        assert!(error.contains("gone.m3u8"), "{error}");
    }

    #[test]
    fn a_playlist_with_nothing_left_to_play_says_so_rather_than_playing_nothing() {
        let directory = tempdir().unwrap();
        let stale = directory.path().join("stale.m3u8");
        fs::write(&stale, "#EXTM3U\ngone.mp4\n").unwrap();
        let empty = directory.path().join("empty.m3u8");
        fs::write(&empty, "#EXTM3U\n").unwrap();

        for playlist in [&stale, &empty] {
            let error = videos(playlist).unwrap_err();
            let name = playlist.file_name().unwrap().to_string_lossy();
            assert!(error.contains(name.as_ref()), "{error}");
            assert!(error.contains("no videos"), "{error}");
        }
    }

    #[test]
    fn collecting_a_folders_videos_finds_supported_videos_recursively() {
        let root = tempdir().unwrap();
        let beach = root.path().join("beach.mp4");
        let notes = root.path().join("notes.txt");
        fs::write(&beach, b"test").unwrap();
        fs::write(&notes, b"test").unwrap();
        let sub = root.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let party = sub.join("party.mkv");
        let hidden = sub.join("hidden.avi");
        fs::write(&party, b"test").unwrap();
        fs::write(&hidden, b"test").unwrap();
        let deep = sub.join("deep");
        fs::create_dir(&deep).unwrap();
        let extra = deep.join("extra.webm");
        fs::write(&extra, b"test").unwrap();

        let mut collected = collect_directory_videos(root.path()).unwrap();
        collected.sort();
        let mut expected = vec![beach, party, hidden, extra];
        expected.sort();
        assert_eq!(collected, expected);
    }

    #[test]
    fn collecting_a_folders_videos_is_case_insensitive() {
        let directory = tempdir().unwrap();
        let upper = directory.path().join("CLIP.MP4");
        fs::write(&upper, b"test").unwrap();
        let collected = collect_directory_videos(directory.path()).unwrap();
        assert_eq!(collected, vec![upper]);
    }

    #[test]
    fn an_empty_folder_reports_no_videos() {
        let directory = tempdir().unwrap();
        let error = collect_directory_videos(directory.path()).unwrap_err();
        assert!(error.contains("no videos"), "{error}");
    }

    #[test]
    fn a_folder_with_only_unsupported_files_reports_no_videos() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("notes.txt"), b"test").unwrap();
        let error = collect_directory_videos(directory.path()).unwrap_err();
        assert!(error.contains("no videos"), "{error}");
    }

    #[test]
    fn shuffle_paths_keeps_all_entries() {
        let mut paths = vec![
            PathBuf::from("/a.mp4"),
            PathBuf::from("/b.mp4"),
            PathBuf::from("/c.mp4"),
        ];
        let original = paths.clone();
        shuffle_paths(&mut paths);
        let mut sorted_original = original.clone();
        let mut sorted_shuffled = paths.clone();
        sorted_original.sort();
        sorted_shuffled.sort();
        assert_eq!(sorted_original, sorted_shuffled);
    }
}
