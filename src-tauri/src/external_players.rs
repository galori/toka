//! Handing a search off to another video player.
//!
//! Players take a playlist file far more reliably than a long argument list,
//! and `m3u8` is the format every player tried here understands: plain text,
//! one path per line, no metadata of any kind.

use serde::Serialize;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// How many videos a handoff carries. A search can match thousands, and no
/// player is improved by being given all of them at once.
pub const MAX_ENTRIES: usize = 100;

/// The players Toka offers, in the order it offers them, each named by the
/// command that launches it. A player only appears if that command is on the
/// PATH, so the list is whatever this computer actually has.
const KNOWN_PLAYERS: &[(&str, &str)] = &[
    ("vlc", "VLC"),
    ("mpv", "mpv"),
    ("celluloid", "Celluloid"),
    ("smplayer", "SMPlayer"),
    ("haruna", "Haruna"),
    ("totem", "Videos"),
    ("mplayer", "MPlayer"),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalPlayer {
    pub command: String,
    pub name: String,
}

/// The known players `installed` reports are present, in offer order.
pub fn available(installed: impl Fn(&str) -> bool) -> Vec<ExternalPlayer> {
    KNOWN_PLAYERS
        .iter()
        .filter(|(command, _)| installed(command))
        .map(|(command, name)| ExternalPlayer {
            command: (*command).to_owned(),
            name: (*name).to_owned(),
        })
        .collect()
}

/// Whether `command` is an executable on `path`, which is the PATH variable
/// split the way the platform splits it.
pub fn on_path(command: &str, path: Option<OsString>) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        let candidate = directory.join(command);
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

/// The body of the playlist file: one path per line, capped at `MAX_ENTRIES`.
///
/// A path holding a line break cannot be written to a format whose only
/// structure is the line break, so such a path is left out rather than
/// silently splitting into two entries that name nothing.
pub fn playlist_body(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .filter(|line| !line.contains(['\n', '\r']))
        .take(MAX_ENTRIES)
        .map(|line| line + "\n")
        .collect()
}

/// Whether `command` is one Toka offers. The frontend sends back what it was
/// given, and this is what keeps that from becoming "run anything".
pub fn is_known(command: &str) -> bool {
    KNOWN_PLAYERS.iter().any(|(known, _)| *known == command)
}

/// Writes the playlist somewhere the player can read it. The name is unique per
/// handoff, so opening a second playlist cannot rewrite the file the first
/// player is still reading.
pub fn write_playlist(directory: &Path, body: &str, id: &str) -> std::io::Result<PathBuf> {
    let path = directory.join(format!("toka-playlist-{id}.m3u8"));
    std::fs::write(&path, body)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn only_installed_players_are_offered_and_the_order_is_kept() {
        let players = available(|command| matches!(command, "mpv" | "vlc"));

        assert_eq!(
            players,
            vec![
                ExternalPlayer {
                    command: "vlc".into(),
                    name: "VLC".into()
                },
                ExternalPlayer {
                    command: "mpv".into(),
                    name: "mpv".into()
                },
            ]
        );
    }

    #[test]
    fn no_players_are_offered_when_none_are_installed() {
        assert!(available(|_| false).is_empty());
    }

    #[test]
    fn the_playlist_is_one_path_per_line() {
        let body = playlist_body(&[
            PathBuf::from("/Videos/first clip.mp4"),
            PathBuf::from("/Videos/second.mkv"),
        ]);

        assert_eq!(body, "/Videos/first clip.mp4\n/Videos/second.mkv\n");
    }

    #[test]
    fn the_playlist_stops_at_the_hundredth_video() {
        let paths: Vec<PathBuf> = (0..250)
            .map(|number| PathBuf::from(format!("/Videos/clip-{number}.mp4")))
            .collect();

        let body = playlist_body(&paths);

        assert_eq!(body.lines().count(), MAX_ENTRIES);
        assert_eq!(body.lines().next(), Some("/Videos/clip-0.mp4"));
        assert_eq!(body.lines().last(), Some("/Videos/clip-99.mp4"));
    }

    #[test]
    fn a_path_holding_a_line_break_is_left_out_rather_than_split() {
        let body = playlist_body(&[
            PathBuf::from("/Videos/sane.mp4"),
            PathBuf::from("/Videos/two\nlines.mp4"),
        ]);

        assert_eq!(body, "/Videos/sane.mp4\n");
    }

    #[test]
    fn only_the_players_toka_offers_are_known() {
        assert!(is_known("vlc"));
        assert!(!is_known("rm"));
        assert!(!is_known(""));
    }

    #[test]
    fn a_command_is_on_the_path_only_where_it_is_executable() {
        let directory = tempdir().unwrap();
        let player = directory.path().join("vlc");
        fs::write(&player, b"#!/bin/sh\n").unwrap();
        let path = Some(OsString::from(directory.path()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(!on_path("vlc", path.clone()));
            fs::set_permissions(&player, fs::Permissions::from_mode(0o755)).unwrap();
        }

        assert!(on_path("vlc", path.clone()));
        assert!(!on_path("mpv", path));
        assert!(!on_path("vlc", None));
    }

    #[test]
    fn each_handoff_writes_its_own_playlist_file() {
        let directory = tempdir().unwrap();

        let first = write_playlist(directory.path(), "/Videos/one.mp4\n", "aaa").unwrap();
        let second = write_playlist(directory.path(), "/Videos/two.mp4\n", "bbb").unwrap();

        assert_ne!(first, second);
        assert_eq!(fs::read_to_string(&first).unwrap(), "/Videos/one.mp4\n");
        assert_eq!(fs::read_to_string(&second).unwrap(), "/Videos/two.mp4\n");
        assert_eq!(first.extension().unwrap(), "m3u8");
    }
}
