//! The playlist file Toka was launched with.
//!
//! Extended M3U is the format every player Toka hands a playlist to
//! understands, and reading one back is the same few lines as writing it: the
//! format carries no structure beyond one entry per line, so a playlist made
//! anywhere — by Toka's own handoff, by VLC, by hand — opens the same way.

use crate::search::is_supported_video;
use std::{
    borrow::Cow,
    ffi::OsString,
    path::{Path, PathBuf},
};

/// The extensions a playlist file goes by. `m3u8` is the UTF-8 one Toka writes;
/// `m3u` is the same format under its older name.
const PLAYLIST_EXTENSIONS: [&str; 2] = ["m3u8", "m3u"];

/// The playlist file Toka was launched with, which is the first argument naming
/// one. `arguments` is the whole command line, program name included, the way
/// the operating system hands it over — including when the viewer opened the
/// playlist in a file manager rather than typing the command themselves.
pub fn from_arguments(arguments: impl IntoIterator<Item = OsString>) -> Option<PathBuf> {
    arguments
        .into_iter()
        .skip(1)
        .map(PathBuf::from)
        .find(|path| is_playlist(path))
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
        .filter(|entry| entry.is_file() && is_supported_video(entry))
        .collect();
    if videos.is_empty() {
        return Err(format!("{} lists no videos Toka can play.", name(path)));
    }
    Ok(videos)
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
        assert_eq!(
            from_arguments(arguments(&["toka", "/Videos/clip.mp4", "--flag"])),
            None
        );
        // The program itself is not the playlist, whatever it is called.
        assert_eq!(from_arguments(arguments(&["/usr/bin/toka.m3u8"])), None);
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
}
