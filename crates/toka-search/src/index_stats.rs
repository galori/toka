//! `toka --index-stats`: a plain-text report of what the indexer knows.

use serde::Serialize;
use std::{fmt::Write as _, path::PathBuf};

/// Where one search folder stands in the indexer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderState {
    Pending,
    Indexing,
    Ready,
    Offline,
    Error,
}

impl FolderState {
    fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Indexing => "indexing",
            Self::Ready => "ready",
            Self::Offline => "offline",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderStats {
    pub path: PathBuf,
    pub state: FolderState,
    pub message: Option<String>,
    /// Files seen so far by a scan that is still running.
    pub scanned_files: u64,
    /// Media indexed in this folder. `None` when it could not be counted,
    /// e.g. an offline folder.
    pub videos: Option<u64>,
    pub images: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexStats {
    pub revision: u64,
    pub videos: u64,
    pub images: u64,
    /// Whether the indexer process/service is running. `None` where the
    /// indexer lives inside the app and so has no process of its own.
    pub service_running: Option<bool>,
    pub folders: Vec<FolderStats>,
}

impl IndexStats {
    fn scanning(&self) -> usize {
        self.folders
            .iter()
            .filter(|folder| folder.state == FolderState::Indexing)
            .count()
    }
}

pub fn render(stats: &IndexStats) -> String {
    let mut out = String::new();
    let scanning = stats.scanning();
    let activity = if scanning > 0 && stats.service_running == Some(false) {
        format!("stalled, {scanning} scan(s) interrupted (service not running)")
    } else if scanning > 0 {
        format!(
            "active, scanning {scanning} folder{}",
            if scanning == 1 { "" } else { "s" }
        )
    } else {
        "idle".to_owned()
    };
    let _ = writeln!(out, "Indexer:  {activity}");
    match stats.service_running {
        Some(true) => {
            let _ = writeln!(out, "Service:  running");
        }
        Some(false) => {
            let _ = writeln!(out, "Service:  not running");
        }
        None => {}
    }
    let _ = writeln!(out, "Revision: {}", stats.revision);
    let _ = writeln!(out, "Videos:   {}", stats.videos);
    let _ = writeln!(out, "Images:   {}", stats.images);
    let _ = writeln!(out, "Folders:  {}", stats.folders.len());
    for folder in &stats.folders {
        let _ = write!(
            out,
            "  {}  [{}]",
            folder.path.display(),
            folder.state.label()
        );
        if let (Some(videos), Some(images)) = (folder.videos, folder.images) {
            let _ = write!(
                out,
                "  {} files ({videos} videos, {images} images)",
                videos + images
            );
        }
        if folder.state == FolderState::Indexing {
            let _ = write!(out, "  {} files scanned so far", folder.scanned_files);
        }
        if let Some(message) = &folder.message {
            let _ = write!(out, "  - {message}");
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(path: &str, state: FolderState) -> FolderStats {
        FolderStats {
            path: PathBuf::from(path),
            state,
            message: None,
            scanned_files: 0,
            videos: Some(0),
            images: Some(0),
        }
    }

    #[test]
    fn idle_report_lists_totals_and_each_folder() {
        let mut movies = folder("/media/movies", FolderState::Ready);
        movies.videos = Some(3);
        movies.images = Some(2);
        let stats = IndexStats {
            revision: 7,
            videos: 3,
            images: 2,
            service_running: Some(true),
            folders: vec![movies],
        };
        let text = render(&stats);
        assert!(text.contains("Indexer:  idle"), "{text}");
        assert!(text.contains("Service:  running"), "{text}");
        assert!(text.contains("Videos:   3"), "{text}");
        assert!(text.contains("Images:   2"), "{text}");
        assert!(
            text.contains("/media/movies  [ready]  5 files (3 videos, 2 images)"),
            "{text}"
        );
    }

    #[test]
    fn folder_states_serialize_to_their_report_labels() {
        for (state, label) in [
            (FolderState::Pending, "pending"),
            (FolderState::Indexing, "indexing"),
            (FolderState::Ready, "ready"),
            (FolderState::Offline, "offline"),
            (FolderState::Error, "error"),
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), label);
        }
    }

    #[test]
    fn stats_serialize_to_the_json_contract() {
        let mut movies = folder("/media/movies", FolderState::Ready);
        movies.videos = Some(3);
        movies.images = Some(2);
        let stats = IndexStats {
            revision: 7,
            videos: 3,
            images: 2,
            service_running: Some(true),
            folders: vec![movies],
        };

        let json = serde_json::to_value(&stats).unwrap();

        assert_eq!(json["revision"], 7);
        assert_eq!(json["videos"], 3);
        assert_eq!(json["images"], 2);
        assert_eq!(json["serviceRunning"], true);
        assert_eq!(json["folders"][0]["path"], "/media/movies");
        assert_eq!(json["folders"][0]["state"], "ready");
        assert_eq!(json["folders"][0]["scannedFiles"], 0);
        assert_eq!(json["folders"][0]["videos"], 3);
        assert_eq!(json["folders"][0]["images"], 2);
    }

    #[test]
    fn scan_with_a_stopped_service_is_reported_as_stalled() {
        let stats = IndexStats {
            revision: 1,
            videos: 0,
            images: 0,
            service_running: Some(false),
            folders: vec![folder("/media/new", FolderState::Indexing)],
        };
        let text = render(&stats);
        assert!(text.contains("Indexer:  stalled"), "{text}");
    }

    #[test]
    fn scanning_folder_makes_the_indexer_active_and_shows_progress() {
        let mut scanning = folder("/media/new", FolderState::Indexing);
        scanning.scanned_files = 42;
        let mut offline = folder("/media/gone", FolderState::Offline);
        offline.videos = None;
        offline.images = None;
        offline.message = Some("not mounted".into());
        let stats = IndexStats {
            revision: 1,
            videos: 0,
            images: 0,
            service_running: Some(true),
            folders: vec![scanning, offline],
        };
        let text = render(&stats);
        assert!(
            text.contains("Indexer:  active, scanning 1 folder\n"),
            "{text}"
        );
        assert!(text.contains("Service:  running"), "{text}");
        assert!(text.contains("42 files scanned so far"), "{text}");
        assert!(
            text.contains("/media/gone  [offline]  - not mounted"),
            "{text}"
        );
    }
}
