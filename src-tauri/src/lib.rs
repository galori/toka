#[cfg(target_os = "linux")]
mod player_linux;
mod providers;
mod search;
mod subtitles;
mod tags;
mod thumbnails;

#[cfg(target_os = "macos")]
use providers::MdfindSearchProvider;
#[cfg(target_os = "linux")]
use providers::{PlocateSearchProvider, RecollSearchProvider};
use search::{SearchEngine, SearchError, SearchPage, SearchProvider, SearchRequest};
use serde::Serialize;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tauri::{Manager, State};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreparedVideo {
    file_path: String,
    playback_backend: &'static str,
    subtitles: Vec<SubtitleTrack>,
}

/// A sidecar subtitle file found beside the video. `track` indexes back into
/// the same detection order, so the frontend never handles a filesystem path.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubtitleTrack {
    track: usize,
    label: String,
    language: Option<String>,
    web_playable: bool,
}

#[derive(Debug, Serialize)]
struct CommandError {
    kind: &'static str,
    message: String,
}

struct DeletedVideo {
    trash_name: String,
}

#[derive(Serialize)]
struct TagsError {
    kind: &'static str,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VideoTagUpdate {
    file_name: String,
    tags: Vec<String>,
}

impl From<SearchError> for CommandError {
    fn from(error: SearchError) -> Self {
        let kind = match &error {
            SearchError::InvalidQuery => "InvalidQuery",
            SearchError::InvalidPage => "InvalidPage",
            SearchError::Provider(_) => "Provider",
            SearchError::VideoUnavailable => "VideoUnavailable",
        };
        Self {
            kind,
            message: error.to_string(),
        }
    }
}

#[tauri::command]
async fn search_videos(
    request: SearchRequest,
    app: tauri::AppHandle,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<SearchPage, CommandError> {
    let engine = Arc::clone(engine.inner());
    tauri::async_runtime::spawn_blocking(move || engine.search(request))
        .await
        .map_err(|error| CommandError {
            kind: "Provider",
            message: format!("The search worker stopped unexpectedly: {error}"),
        })?
        .map_err(Into::into)
        .and_then(|page| {
            for result in &page.results {
                if let Some(path) = &result.thumbnail_path {
                    app.asset_protocol_scope()
                        .allow_file(path)
                        .map_err(|_| CommandError {
                            kind: "Thumbnail",
                            message: "The video thumbnail could not be exposed.".into(),
                        })?;
                }
            }
            Ok(page)
        })
}

#[tauri::command]
fn video_thumbnail(
    result_id: String,
    app: tauri::AppHandle,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<String, CommandError> {
    let path = engine
        .thumbnail_path(&result_id)
        .map_err(CommandError::from)?;
    app.asset_protocol_scope()
        .allow_file(&path)
        .map_err(|_| CommandError {
            kind: "Thumbnail",
            message: "The video thumbnail could not be exposed.".into(),
        })?;
    Ok(path.to_string_lossy().into_owned())
}

fn move_to_trash(path: &std::path::Path) -> Result<String, CommandError> {
    #[cfg(target_os = "linux")]
    let output = std::process::Command::new("gio")
        .args(["trash", path.to_string_lossy().as_ref()])
        .output();
    #[cfg(target_os = "macos")]
    let output = std::process::Command::new("osascript")
        .args([
            "-e",
            &format!(
                "tell application \"Finder\" to delete POSIX file \"{}\"",
                path.display()
            ),
        ])
        .output();
    let output = output.map_err(|error| CommandError {
        kind: "Delete",
        message: format!("The video could not be moved to the trash: {error}"),
    })?;
    trash_outcome(
        path,
        output.status.success(),
        &String::from_utf8_lossy(&output.stderr),
        // Checked after the helper has run, and deliberately not through
        // `exists`: a symlink that still dangles was not trashed either.
        path.symlink_metadata().is_ok(),
    )
}

/// What a trash helper's result means for the file it was handed. A helper that
/// reports success and leaves the file where it was has deleted nothing, and
/// saying otherwise is what let a video disappear from the playlist while it
/// stayed on disk.
fn trash_outcome(
    path: &std::path::Path,
    succeeded: bool,
    stderr: &str,
    still_on_disk: bool,
) -> Result<String, CommandError> {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    if !succeeded {
        let detail = stderr.trim();
        return Err(CommandError {
            kind: "Delete",
            message: if detail.is_empty() {
                format!("{name} could not be moved to the trash.")
            } else {
                detail.to_owned()
            },
        });
    }
    if still_on_disk {
        return Err(CommandError {
            kind: "Delete",
            message: format!(
                "{name} is still on disk: the trash helper reported success without moving it."
            ),
        });
    }
    Ok(name)
}

#[tauri::command]
fn delete_video(
    result_id: String,
    engine: State<'_, Arc<SearchEngine>>,
    deleted: State<'_, Mutex<Option<DeletedVideo>>>,
) -> Result<(), CommandError> {
    let path = engine.video_path(&result_id).map_err(CommandError::from)?;
    let trash_name = move_to_trash(&path)?;
    *deleted.lock().unwrap() = Some(DeletedVideo { trash_name });
    Ok(())
}

#[tauri::command]
fn undo_delete(deleted: State<'_, Mutex<Option<DeletedVideo>>>) -> Result<(), CommandError> {
    let item = deleted.lock().unwrap().take().ok_or_else(|| CommandError {
        kind: "Delete",
        message: "There is no video deletion to undo.".into(),
    })?;
    #[cfg(target_os = "linux")]
    let output = std::process::Command::new("gio")
        .args([
            "trash",
            "--restore",
            &format!("trash:///{}", item.trash_name),
        ])
        .output();
    #[cfg(target_os = "macos")]
    let output = std::process::Command::new("osascript")
        .args([
            "-e",
            &format!(
                "tell application \"Finder\" to move POSIX file \"{}\" to original location",
                item.trash_name
            ),
        ])
        .output();
    let output = output.map_err(|error| CommandError {
        kind: "Delete",
        message: format!("The deleted video could not be restored: {error}"),
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(CommandError {
            kind: "Delete",
            message: "The deleted video could not be restored.".into(),
        })
    }
}

#[tauri::command]
fn set_video_tags(
    result_id: String,
    tags: Vec<String>,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<VideoTagUpdate, TagsError> {
    update_video_tags(&result_id, &tags, &engine, tags::set)
}

#[tauri::command]
fn add_video_tags(
    result_id: String,
    tags: Vec<String>,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<VideoTagUpdate, TagsError> {
    update_video_tags(&result_id, &tags, &engine, tags::add)
}

#[tauri::command]
fn remove_video_tags(
    result_id: String,
    tags: Vec<String>,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<VideoTagUpdate, TagsError> {
    update_video_tags(&result_id, &tags, &engine, tags::remove)
}

/// Retags the video behind `result_id` and keeps the search engine pointing at
/// the renamed file.
fn update_video_tags(
    result_id: &str,
    tags: &[String],
    engine: &SearchEngine,
    operation: fn(&Path, &[String]) -> Result<tags::TagUpdate, String>,
) -> Result<VideoTagUpdate, TagsError> {
    let path = engine.video_path(result_id).map_err(|error| TagsError {
        kind: "VideoUnavailable",
        message: error.to_string(),
    })?;
    let update = operation(&path, tags).map_err(|message| TagsError {
        kind: "Tags",
        message,
    })?;
    engine
        .update_video_path(result_id, update.path.clone())
        .map_err(|error| TagsError {
            kind: "VideoUnavailable",
            message: error.to_string(),
        })?;
    Ok(VideoTagUpdate {
        file_name: update
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        tags: update.tags,
    })
}

#[tauri::command]
fn prepare_video(
    result_id: String,
    app: tauri::AppHandle,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<PreparedVideo, CommandError> {
    let path = engine.video_path(&result_id).map_err(CommandError::from)?;
    app.asset_protocol_scope()
        .allow_file(&path)
        .map_err(|_| CommandError::from(SearchError::VideoUnavailable))?;
    let subtitles = subtitles::sidecar_subtitles(&path)
        .into_iter()
        .enumerate()
        .map(|(track, subtitle)| SubtitleTrack {
            track,
            label: subtitle.label,
            language: subtitle.language,
            web_playable: subtitle.web_playable,
        })
        .collect();
    Ok(PreparedVideo {
        file_path: path.to_string_lossy().into_owned(),
        playback_backend: if cfg!(all(
            target_os = "linux",
            any(not(feature = "e2e"), feature = "native-e2e")
        )) {
            "native"
        } else {
            "web"
        },
        subtitles,
    })
}

/// WebVTT cues for a sidecar subtitle, so the web media engine can attach a
/// text track without the frontend ever seeing a filesystem path.
#[tauri::command]
fn subtitle_cues(
    result_id: String,
    track: usize,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<String, CommandError> {
    let path = engine.video_path(&result_id).map_err(CommandError::from)?;
    let subtitle = subtitles::sidecar_subtitles(&path)
        .into_iter()
        .nth(track)
        .ok_or_else(|| CommandError {
            kind: "Subtitle",
            message: "That subtitle file is no longer beside the video.".into(),
        })?;
    let extension = subtitle
        .path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_owned();
    let source = std::fs::read_to_string(&subtitle.path).map_err(|error| CommandError {
        kind: "Subtitle",
        message: format!("The subtitle file could not be read: {error}"),
    })?;
    subtitles::to_web_vtt(&source, &extension).ok_or_else(|| CommandError {
        kind: "Subtitle",
        message: format!(
            "{} subtitles are not supported by this player.",
            subtitle.label
        ),
    })
}

#[cfg(target_os = "linux")]
fn playback_error(message: String) -> CommandError {
    CommandError {
        kind: "Playback",
        message,
    }
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn load_native_video(
    file_path: String,
    player: State<'_, Arc<player_linux::NativePlayer>>,
) -> Result<(), CommandError> {
    player_linux::load(player.inner(), &file_path).map_err(playback_error)
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn set_native_paused(
    paused: bool,
    player: State<'_, Arc<player_linux::NativePlayer>>,
) -> Result<(), CommandError> {
    player_linux::set_paused(player.inner(), paused).map_err(playback_error)
}
#[cfg(target_os = "linux")]
#[tauri::command]
fn set_native_speed(
    speed: f64,
    player: State<'_, Arc<player_linux::NativePlayer>>,
) -> Result<(), CommandError> {
    player_linux::set_speed(player.inner(), speed).map_err(playback_error)
}
#[cfg(target_os = "linux")]
#[tauri::command]
fn set_native_volume(
    volume: f64,
    player: State<'_, Arc<player_linux::NativePlayer>>,
) -> Result<(), CommandError> {
    player_linux::set_volume(player.inner(), volume).map_err(playback_error)
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn native_video_rotation(
    player: State<'_, Arc<player_linux::NativePlayer>>,
) -> Result<i32, CommandError> {
    player_linux::rotation(player.inner()).map_err(playback_error)
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn set_native_video_rotation(
    degrees: i32,
    player: State<'_, Arc<player_linux::NativePlayer>>,
) -> Result<(), CommandError> {
    player_linux::set_rotation(player.inner(), degrees).map_err(playback_error)
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn native_subtitle_tracks(
    player: State<'_, Arc<player_linux::NativePlayer>>,
) -> Result<Vec<player_linux::SubtitleTrack>, CommandError> {
    player_linux::subtitle_tracks(player.inner()).map_err(playback_error)
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn set_native_subtitle(
    id: Option<i64>,
    player: State<'_, Arc<player_linux::NativePlayer>>,
) -> Result<(), CommandError> {
    player_linux::set_subtitle(player.inner(), id).map_err(playback_error)
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn seek_native_video(
    seconds: f64,
    player: State<'_, Arc<player_linux::NativePlayer>>,
) -> Result<(), CommandError> {
    player_linux::seek(player.inner(), seconds).map_err(playback_error)
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn native_playback_state(
    player: State<'_, Arc<player_linux::NativePlayer>>,
) -> Result<player_linux::PlaybackState, CommandError> {
    player_linux::state(player.inner()).map_err(playback_error)
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn stop_native_video(
    player: State<'_, Arc<player_linux::NativePlayer>>,
) -> Result<(), CommandError> {
    player_linux::stop(player.inner()).map_err(playback_error)
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn set_native_video_bounds(x: i32, y: i32, width: i32, height: i32, visible: bool) {
    player_linux::set_bounds(x, y, width, height, visible);
}

fn platform_provider() -> Arc<dyn SearchProvider> {
    #[cfg(feature = "e2e")]
    {
        struct FixtureSearchProvider;
        impl SearchProvider for FixtureSearchProvider {
            fn candidates(&self, _query: &str) -> Result<Vec<std::path::PathBuf>, SearchError> {
                let paths = std::env::var_os("TOKA_E2E_VIDEOS").ok_or_else(|| {
                    SearchError::Provider("The integration-test videos were not configured.".into())
                })?;
                Ok(std::env::split_paths(&paths).collect())
            }
        }
        if std::env::var_os("TOKA_SEARCH_PROVIDER").is_none() {
            return Arc::new(FixtureSearchProvider);
        }
    }
    #[cfg(target_os = "macos")]
    {
        Arc::new(MdfindSearchProvider::system())
    }
    #[cfg(target_os = "linux")]
    {
        match std::env::var("TOKA_SEARCH_PROVIDER").as_deref() {
            Ok("recoll") => Arc::new(RecollSearchProvider::system()),
            Ok("plocate") | Err(_) => Arc::new(PlocateSearchProvider::system()),
            Ok(name) => Arc::new(InvalidProvider(format!(
                "Unknown Linux search provider: {name}"
            ))),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        struct UnsupportedProvider;
        impl SearchProvider for UnsupportedProvider {
            fn candidates(&self, _query: &str) -> Result<Vec<std::path::PathBuf>, SearchError> {
                Err(SearchError::Provider(
                    "Toka currently supports video search on macOS and Linux.".into(),
                ))
            }
        }
        Arc::new(UnsupportedProvider)
    }
}

#[cfg(target_os = "linux")]
struct InvalidProvider(String);

#[cfg(target_os = "linux")]
impl SearchProvider for InvalidProvider {
    fn candidates(&self, _query: &str) -> Result<Vec<std::path::PathBuf>, SearchError> {
        Err(SearchError::Provider(self.0.clone()))
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    #[cfg(feature = "e2e")]
    let builder = builder
        .plugin(tauri_plugin_wdio::init())
        .plugin(tauri_plugin_wdio_webdriver::init());

    let builder = builder
        .manage(Arc::new(SearchEngine::new(platform_provider())))
        .manage(Mutex::new(None::<DeletedVideo>));
    #[cfg(target_os = "linux")]
    let builder = if cfg!(all(feature = "e2e", not(feature = "native-e2e"))) {
        builder.invoke_handler(tauri::generate_handler![
            search_videos,
            video_thumbnail,
            delete_video,
            undo_delete,
            set_video_tags,
            add_video_tags,
            remove_video_tags,
            prepare_video,
            subtitle_cues
        ])
    } else {
        let player = player_linux::NativePlayer::new();
        let setup_player = player.clone();
        builder
            .manage(player)
            .setup(move |app| player_linux::install(app, setup_player.clone()))
            .invoke_handler(tauri::generate_handler![
                search_videos,
                video_thumbnail,
                delete_video,
                undo_delete,
                set_video_tags,
                add_video_tags,
                remove_video_tags,
                prepare_video,
                subtitle_cues,
                load_native_video,
                set_native_paused,
                set_native_speed,
                set_native_volume,
                native_video_rotation,
                set_native_video_rotation,
                seek_native_video,
                native_subtitle_tracks,
                set_native_subtitle,
                native_playback_state,
                stop_native_video,
                set_native_video_bounds
            ])
    };
    #[cfg(not(target_os = "linux"))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        search_videos,
        video_thumbnail,
        delete_video,
        undo_delete,
        set_video_tags,
        add_video_tags,
        remove_video_tags,
        prepare_video,
        subtitle_cues
    ]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running Toka");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn outcome(succeeded: bool, stderr: &str, still_on_disk: bool) -> Result<String, CommandError> {
        trash_outcome(
            Path::new("/Videos/clip.mp4"),
            succeeded,
            stderr,
            still_on_disk,
        )
    }

    #[test]
    fn a_trashed_video_reports_the_name_it_went_to_the_trash_under() {
        assert_eq!(outcome(true, "", false).unwrap(), "clip.mp4");
    }

    #[test]
    fn a_helper_that_reported_success_without_moving_the_file_is_a_failure() {
        let error = outcome(true, "", true).unwrap_err();

        assert_eq!(error.kind, "Delete");
        assert!(error.message.contains("still on disk"), "{}", error.message);
    }

    #[test]
    fn a_failed_helper_passes_on_what_it_said_went_wrong() {
        let error = outcome(false, "  Trashing is not supported here\n", true).unwrap_err();

        assert_eq!(error.message, "Trashing is not supported here");
    }

    #[test]
    fn a_failed_helper_that_said_nothing_still_names_the_video() {
        let error = outcome(false, "   ", true).unwrap_err();

        assert!(error.message.contains("clip.mp4"), "{}", error.message);
    }
}
