mod external_players;
#[cfg(target_os = "linux")]
mod managed_index;
#[cfg(target_os = "linux")]
mod player_linux;
mod playlist;
mod providers;
mod search;
mod subtitles;
mod tags;
mod thumbnails;

#[cfg(target_os = "macos")]
use providers::MdfindSearchProvider;
#[cfg(target_os = "linux")]
use providers::{ManagedPlocateSearchProvider, PlocateSearchProvider, RecollSearchProvider};
use search::{SearchEngine, SearchError, SearchPage, SearchProvider, SearchRequest};
use serde::Serialize;
use std::path::{Path, PathBuf};
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

/// The playlist file Toka was launched with, kept from the command line until
/// the frontend is up to ask for it.
struct LaunchPlaylist(Option<PathBuf>);

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

/// One video out of a page that was tagged in a single go, under the id the
/// frontend knows it by — tagging renames the file, so the caller cannot match
/// these up by name afterwards.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaggedVideo {
    result_id: String,
    file_name: String,
    tags: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BulkTagUpdate {
    tagged: Vec<TaggedVideo>,
    failed: usize,
    /// Why the first video that could not be tagged was left alone. A page of
    /// identical failures says no more than the first one of them does.
    problem: Option<String>,
}

#[cfg(target_os = "linux")]
fn index_error(message: String) -> CommandError {
    CommandError {
        kind: "Index",
        message,
    }
}

#[cfg(all(target_os = "linux", not(feature = "e2e")))]
#[tauri::command]
fn index_state(
    manager: State<'_, managed_index::IndexManager>,
) -> Result<managed_index::IndexState, CommandError> {
    manager.state().map_err(index_error)
}

// Integration builds use a fixture search provider rather than a persistent
// Linux index. Reporting that index management is unsupported keeps the
// fixture's search screen deterministic and prevents tests from writing the
// developer's real folder configuration.
#[cfg(all(target_os = "linux", feature = "e2e"))]
#[tauri::command]
fn index_state() -> managed_index::IndexState {
    managed_index::IndexState {
        supported: false,
        revision: 0,
        folders: Vec::new(),
    }
}

#[cfg(not(target_os = "linux"))]
#[derive(Serialize)]
struct UnsupportedIndexState {
    supported: bool,
    revision: u64,
    folders: Vec<String>,
}

#[cfg(not(target_os = "linux"))]
#[tauri::command]
fn index_state() -> UnsupportedIndexState {
    UnsupportedIndexState {
        supported: false,
        revision: 0,
        folders: Vec::new(),
    }
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn add_index_folder(
    path: String,
    manager: State<'_, managed_index::IndexManager>,
) -> Result<managed_index::IndexState, CommandError> {
    manager.add_folder(Path::new(&path)).map_err(index_error)
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn remove_index_folder(
    id: String,
    manager: State<'_, managed_index::IndexManager>,
) -> Result<managed_index::IndexState, CommandError> {
    manager.remove_folder(&id).map_err(index_error)
}

impl From<SearchError> for CommandError {
    fn from(error: SearchError) -> Self {
        let kind = match &error {
            SearchError::InvalidQuery => "InvalidQuery",
            SearchError::NoSearchFields => "NoSearchFields",
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

/// The frames a result's preview runs through while the pointer rests on it.
/// Asked for on hover rather than with the search, because generating them is
/// several seeks through the file and most results are never pointed at.
#[tauri::command]
async fn video_preview(
    result_id: String,
    app: tauri::AppHandle,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<Vec<String>, CommandError> {
    // On a worker, not the main thread: this runs ffmpeg once per frame, and
    // on the main thread a first hover would freeze the window for as long as
    // that took.
    let engine = Arc::clone(engine.inner());
    let paths = tauri::async_runtime::spawn_blocking(move || engine.preview_paths(&result_id))
        .await
        .map_err(|error| CommandError {
            kind: "Thumbnail",
            message: format!("The preview worker stopped unexpectedly: {error}"),
        })?
        .map_err(CommandError::from)?;
    let mut sources = Vec::with_capacity(paths.len());
    for path in paths {
        app.asset_protocol_scope()
            .allow_file(&path)
            .map_err(|_| CommandError {
                kind: "Thumbnail",
                message: "The video preview could not be exposed.".into(),
            })?;
        sources.push(path.to_string_lossy().into_owned());
    }
    Ok(sources)
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

/// The playlist — or single video, or folder — Toka was launched with, as a
/// page of results to play, so it goes through the same player, drawer and
/// controls a search's results do.
///
/// `toka summer.m3u8` opens that playlist, `toka clip.mp4` plays that one
/// video, and `toka /Videos` plays every supported video under that folder and
/// its subfolders, shuffled like any other playlist.
///
/// `None` is the ordinary case: Toka was started on its own and belongs on an
/// empty search. A playlist, video or folder that cannot be read, and one with
/// nothing left in it to play, come back as errors for the viewer to see
/// instead.
#[tauri::command]
fn launch_playlist(
    launched: State<'_, LaunchPlaylist>,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<Option<SearchPage>, CommandError> {
    let Some(path) = launched.0.as_deref() else {
        return Ok(None);
    };
    if path.is_dir() {
        let mut videos =
            playlist::collect_directory_videos(path).map_err(|message| CommandError {
                kind: "Playlist",
                message,
            })?;
        playlist::shuffle_paths(&mut videos);
        let name = playlist::name(path).into_owned();
        return Ok(Some(engine.page_of_videos(name, videos)));
    }
    if search::is_supported_video(path) {
        if !path.is_file() {
            return Err(CommandError {
                kind: "Playlist",
                message: format!("{} could not be found.", playlist::name(path)),
            });
        }
        let name = playlist::name(path).into_owned();
        return Ok(Some(engine.page_of_videos(name, vec![path.to_path_buf()])));
    }
    let videos = playlist::videos(path).map_err(|message| CommandError {
        kind: "Playlist",
        message,
    })?;
    let name = playlist::name(path).into_owned();
    Ok(Some(engine.page_of_videos(name, videos)))
}

/// The players installed on this computer that Toka can hand a playlist to.
#[tauri::command]
fn external_players() -> Vec<external_players::ExternalPlayer> {
    external_players::available(|command| {
        external_players::on_path(command, std::env::var_os("PATH"))
    })
}

/// Writes the search results to a playlist file and opens it in `player`.
#[tauri::command]
fn open_in_external_player(
    player: String,
    result_ids: Vec<String>,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<usize, CommandError> {
    // The frontend sends back a command it was given, and this is what keeps
    // that from becoming a way to run anything at all.
    if !external_players::is_known(&player) {
        return Err(CommandError {
            kind: "ExternalPlayer",
            message: format!("{player} is not a video player Toka can open."),
        });
    }
    let paths: Vec<std::path::PathBuf> = result_ids
        .iter()
        .take(external_players::MAX_ENTRIES)
        .filter_map(|result_id| engine.video_path(result_id).ok())
        .collect();
    let body = external_players::playlist_body(&paths);
    let entries = body.lines().count();
    if entries == 0 {
        return Err(CommandError {
            kind: "ExternalPlayer",
            message: "None of these videos are still available to open.".into(),
        });
    }
    let playlist = external_players::write_playlist(
        &std::env::temp_dir(),
        &body,
        &uuid::Uuid::new_v4().to_string(),
    )
    .map_err(|error| CommandError {
        kind: "ExternalPlayer",
        message: format!("The playlist could not be written: {error}"),
    })?;
    std::process::Command::new(&player)
        .arg(&playlist)
        .spawn()
        .map_err(|error| CommandError {
            kind: "ExternalPlayer",
            message: format!("{player} could not be started: {error}"),
        })?;
    Ok(entries)
}

/// Starts another Toka. The desktop launcher raises the one already running
/// rather than opening a second, so the running one has to be able to ask.
#[tauri::command]
fn open_new_window() -> Result<(), CommandError> {
    let executable = std::env::current_exe().map_err(|error| CommandError {
        kind: "NewWindow",
        message: format!("Toka could not find its own program to start again: {error}"),
    })?;
    std::process::Command::new(&executable)
        .spawn()
        .map_err(|error| CommandError {
            kind: "NewWindow",
            message: format!("Another Toka could not be started: {error}"),
        })?;
    Ok(())
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

/// Puts the same tags on every video in `result_ids`, in one call rather than
/// one call each: a page of results is hundreds of videos, and tagging is a
/// rename, so this has to walk them one at a time behind a single request.
#[tauri::command]
fn add_tags_to_videos(
    result_ids: Vec<String>,
    tags: Vec<String>,
    engine: State<'_, Arc<SearchEngine>>,
) -> BulkTagUpdate {
    tag_videos(&result_ids, &tags, &engine)
}

fn tag_videos(result_ids: &[String], tags: &[String], engine: &SearchEngine) -> BulkTagUpdate {
    let mut tagged = Vec::new();
    let mut failed = 0;
    let mut problem = None;
    for result_id in result_ids {
        // One video that has moved or been deleted since the search is a
        // reason to say so afterwards, not a reason to abandon the rest of
        // the page half tagged.
        match update_video_tags(result_id, tags, engine, tags::add) {
            Ok(update) => tagged.push(TaggedVideo {
                result_id: result_id.clone(),
                file_name: update.file_name,
                tags: update.tags,
            }),
            Err(error) => {
                failed += 1;
                if problem.is_none() {
                    problem = Some(error.message);
                }
            }
        }
    }
    BulkTagUpdate {
        tagged,
        failed,
        problem,
    }
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
fn set_native_video_aspect(
    ratio: f64,
    player: State<'_, Arc<player_linux::NativePlayer>>,
) -> Result<(), CommandError> {
    player_linux::set_aspect(player.inner(), ratio).map_err(playback_error)
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
            fn candidates(
                &self,
                _query: &str,
                _whole_path: bool,
            ) -> Result<Vec<std::path::PathBuf>, SearchError> {
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
            Ok("plocate") => Arc::new(PlocateSearchProvider::system()),
            Err(_) => Arc::new(ManagedPlocateSearchProvider::system(
                managed_index::IndexPaths::system()
                    .expect("Toka could not find its private index paths"),
            )),
            Ok(name) => Arc::new(InvalidProvider(format!(
                "Unknown Linux search provider: {name}"
            ))),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        struct UnsupportedProvider;
        impl SearchProvider for UnsupportedProvider {
            fn candidates(
                &self,
                _query: &str,
                _whole_path: bool,
            ) -> Result<Vec<std::path::PathBuf>, SearchError> {
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
    fn candidates(
        &self,
        _query: &str,
        _whole_path: bool,
    ) -> Result<Vec<std::path::PathBuf>, SearchError> {
        Err(SearchError::Provider(self.0.clone()))
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--indexer")) {
        let paths = managed_index::IndexPaths::system().unwrap_or_else(|message| {
            eprintln!("{message}");
            std::process::exit(1);
        });
        if let Err(message) = managed_index::run_daemon(paths) {
            eprintln!("{message}");
            std::process::exit(1);
        }
        return;
    }
    let builder = tauri::Builder::default().plugin(tauri_plugin_dialog::init());
    #[cfg(feature = "e2e")]
    let builder = builder
        .plugin(tauri_plugin_wdio::init())
        .plugin(tauri_plugin_wdio_webdriver::init());

    // Read before the window exists, because the command line Toka was started
    // with is the whole of what it was asked to open.
    let launched = LaunchPlaylist(playlist::from_arguments(std::env::args_os()));
    let builder = builder
        .manage(Arc::new(SearchEngine::new(platform_provider())))
        .manage(Mutex::new(None::<DeletedVideo>))
        .manage(launched);
    #[cfg(target_os = "linux")]
    let builder = builder.manage(
        managed_index::IndexManager::system()
            .expect("Toka could not find its private index settings"),
    );
    #[cfg(target_os = "linux")]
    let builder = if cfg!(all(feature = "e2e", not(feature = "native-e2e"))) {
        builder.invoke_handler(tauri::generate_handler![
            search_videos,
            index_state,
            add_index_folder,
            remove_index_folder,
            launch_playlist,
            video_thumbnail,
            video_preview,
            delete_video,
            undo_delete,
            external_players,
            open_in_external_player,
            open_new_window,
            set_video_tags,
            add_video_tags,
            remove_video_tags,
            add_tags_to_videos,
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
                index_state,
                add_index_folder,
                remove_index_folder,
                launch_playlist,
                video_thumbnail,
                video_preview,
                delete_video,
                undo_delete,
                external_players,
                open_in_external_player,
                open_new_window,
                set_video_tags,
                add_video_tags,
                remove_video_tags,
                add_tags_to_videos,
                prepare_video,
                subtitle_cues,
                load_native_video,
                set_native_paused,
                set_native_speed,
                set_native_volume,
                native_video_rotation,
                set_native_video_rotation,
                set_native_video_aspect,
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
        index_state,
        launch_playlist,
        video_thumbnail,
        video_preview,
        delete_video,
        undo_delete,
        external_players,
        open_in_external_player,
        open_new_window,
        set_video_tags,
        add_video_tags,
        remove_video_tags,
        add_tags_to_videos,
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

    #[cfg(all(target_os = "linux", feature = "e2e"))]
    #[test]
    fn fixture_search_builds_do_not_open_real_folder_setup() {
        assert!(!index_state().supported);
    }

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

    struct FakeProvider(Vec<PathBuf>);

    impl SearchProvider for FakeProvider {
        fn candidates(&self, _query: &str, _whole_path: bool) -> Result<Vec<PathBuf>, SearchError> {
            Ok(self.0.clone())
        }
    }

    /// An engine holding a page of results for `clip`, so the ids a bulk tag
    /// works from are the ones a real search would have handed out.
    fn engine_over(paths: Vec<PathBuf>) -> (Arc<SearchEngine>, Vec<String>) {
        let engine = Arc::new(SearchEngine::new(Arc::new(FakeProvider(paths))));
        let page = engine
            .search(SearchRequest {
                query: "clip".into(),
                page: 1,
                page_size: search::PAGE_SIZE,
                fields: Default::default(),
                media_type: Default::default(),
            })
            .unwrap();
        let ids = page.results.iter().map(|r| r.id.clone()).collect();
        (engine, ids)
    }

    #[test]
    fn tagging_a_page_puts_the_same_tags_on_every_video_in_it() {
        let directory = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = ["clip-one.mp4", "clip-two.mp4"]
            .iter()
            .map(|name| {
                let path = directory.path().join(name);
                std::fs::write(&path, b"test").unwrap();
                path
            })
            .collect();
        let (engine, ids) = engine_over(paths);

        let update = tag_videos(&ids, &["beach".to_owned()], &engine);

        assert_eq!(update.failed, 0);
        assert_eq!(update.problem, None);
        assert_eq!(update.tagged.len(), 2);
        for tagged in &update.tagged {
            assert_eq!(tagged.tags, ["beach"]);
            assert!(tagged.file_name.contains("[beach]"), "{}", tagged.file_name);
            // Renaming is what tagging is, so the engine has to be pointing at
            // the new name — otherwise the next tag on the same video fails.
            assert!(engine.video_path(&tagged.result_id).is_ok());
        }
    }

    /// A video that has gone since the search should not take the rest of the
    /// page down with it.
    #[test]
    fn tagging_a_page_carries_on_past_a_video_that_is_no_longer_there() {
        let directory = tempfile::tempdir().unwrap();
        let present = directory.path().join("clip-here.mp4");
        let missing = directory.path().join("clip-gone.mp4");
        std::fs::write(&present, b"test").unwrap();
        std::fs::write(&missing, b"test").unwrap();
        let (engine, ids) = engine_over(vec![present, missing.clone()]);
        std::fs::remove_file(&missing).unwrap();

        let update = tag_videos(&ids, &["beach".to_owned()], &engine);

        assert_eq!(update.tagged.len(), 1);
        assert_eq!(update.failed, 1);
        assert!(update.problem.is_some());
    }
}
