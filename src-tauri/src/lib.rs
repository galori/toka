mod addons;
mod cli;
mod external_players;
mod launch;
#[cfg(target_os = "linux")]
mod player_linux;
mod playlist;
mod restart;
mod subtitles;
mod ts_client;

// The search and indexing modules live in the `toka-search` crate. The app
// reaches them only through `ts_client` (which spawns `ts`) and pure
// helpers with no index IO — see the ownership rule on `ts_client`.
use serde::Serialize;
#[cfg(all(target_os = "linux", not(feature = "e2e"), not(debug_assertions)))]
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{Manager, State};
use tauri_plugin_opener::OpenerExt;
use toka_search::{
    owned_index::OwnedIndexState,
    search::{self, SearchEngine, SearchError, SearchPage, SearchProvider, SearchRequest},
    search_log::{SearchLogSettings, SearchLogger},
    tags,
};

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
    original_path: PathBuf,
}

/// What Toka was asked to open, kept until the frontend is ready for it.
struct LaunchRequest(Result<launch::LaunchIntent, String>);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LaunchResponse {
    page: SearchPage,
    addon_search: Option<addons::AddonSearchLaunch>,
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

/// What tagging a video's folder did: the folder's tags now, and every result
/// the engine knows in that folder, since renaming it moved all of them.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FolderTagUpdate {
    folder_tags: Vec<String>,
    result_ids: Vec<String>,
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

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    any(not(feature = "e2e"), feature = "native-e2e")
))]
fn index_error(message: String) -> CommandError {
    CommandError {
        kind: "Index",
        message,
    }
}

#[cfg(all(any(target_os = "linux", target_os = "macos"), not(feature = "e2e")))]
#[tauri::command]
fn index_state() -> Result<OwnedIndexState, CommandError> {
    ts_client::index_state().map_err(index_error)
}

// Integration builds search through a fixture `ts` rather than a
// persistent index. Reporting that index management is unsupported keeps
// the fixture's search screen deterministic and prevents tests from
// writing the developer's real folder configuration.
#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "e2e"))]
#[tauri::command]
fn index_state() -> OwnedIndexState {
    OwnedIndexState {
        supported: false,
        revision: 0,
        indexed_videos: 0,
        indexed_images: 0,
        indexing_files: 0,
        folders: Vec::new(),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[derive(Serialize)]
struct UnsupportedIndexState {
    supported: bool,
    revision: u64,
    indexed_videos: u64,
    indexed_images: u64,
    indexing_files: u64,
    folders: Vec<String>,
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[tauri::command]
fn index_state() -> UnsupportedIndexState {
    UnsupportedIndexState {
        supported: false,
        revision: 0,
        indexed_videos: 0,
        indexed_images: 0,
        indexing_files: 0,
        folders: Vec::new(),
    }
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    any(not(feature = "e2e"), feature = "native-e2e")
))]
#[tauri::command]
fn add_index_folder(path: String) -> Result<OwnedIndexState, CommandError> {
    ts_client::folders_add(Path::new(&path)).map_err(index_error)
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    any(not(feature = "e2e"), feature = "native-e2e")
))]
#[tauri::command]
fn remove_index_folder(id: String) -> Result<OwnedIndexState, CommandError> {
    ts_client::folders_remove_by_id(&id).map_err(index_error)
}

impl From<SearchError> for CommandError {
    fn from(error: SearchError) -> Self {
        let kind = match &error {
            SearchError::InvalidQuery => "InvalidQuery",
            SearchError::NoSearchFields => "NoSearchFields",
            SearchError::InvalidPage => "InvalidPage",
            SearchError::SearchExpired => "SearchExpired",
            SearchError::Provider(_) => "Provider",
            SearchError::VideoUnavailable => "VideoUnavailable",
        };
        Self {
            kind,
            message: error.to_string(),
        }
    }
}

fn addon_error(message: String) -> CommandError {
    CommandError {
        kind: "Addon",
        message,
    }
}

fn search_with_addon(
    request: SearchRequest,
    addon_search: &addons::AddonSearchLaunch,
    engine: &SearchEngine,
    registry: &addons::AddonRegistry,
) -> Result<SearchPage, CommandError> {
    registry
        .validate_related_search(addon_search)
        .map_err(addon_error)?;
    engine
        .search_filtered_for_addon(
            request,
            &addon_search.addon_id,
            &addon_search.capture,
            |path| {
                let file_name = path.file_name().unwrap_or_default().to_string_lossy();
                registry
                    .matches_related_search(addon_search, &file_name)
                    .unwrap_or(false)
            },
        )
        .map_err(Into::into)
}

fn match_count_with_addon(
    request: SearchRequest,
    addon_search: &addons::AddonSearchLaunch,
    engine: &SearchEngine,
    registry: &addons::AddonRegistry,
) -> Result<usize, CommandError> {
    registry
        .validate_related_search(addon_search)
        .map_err(addon_error)?;
    engine
        .match_count_filtered_for_addon(
            request,
            &addon_search.addon_id,
            &addon_search.capture,
            |path| {
                let file_name = path.file_name().unwrap_or_default().to_string_lossy();
                registry
                    .matches_related_search(addon_search, &file_name)
                    .unwrap_or(false)
            },
        )
        .map_err(Into::into)
}

fn addon_search_page(
    addon_search: addons::AddonSearchLaunch,
    engine: &SearchEngine,
    registry: &addons::AddonRegistry,
) -> Result<SearchPage, CommandError> {
    let request = SearchRequest {
        query: addon_search.query.clone(),
        page: 1,
        page_size: search::PAGE_SIZE,
        search_id: None,
        fields: addon_search.fields,
        media_type: addon_search.media_type,
        sort: search::SearchSort::default(),
    };
    search_with_addon(request, &addon_search, engine, registry)
}

#[tauri::command]
fn search_log_settings(logger: State<'_, Arc<SearchLogger>>) -> SearchLogSettings {
    logger.settings()
}

#[tauri::command]
fn set_search_log_settings(
    verbose: bool,
    path: Option<String>,
    logger: State<'_, Arc<SearchLogger>>,
) -> Result<SearchLogSettings, CommandError> {
    logger
        .set_settings(verbose, path.map(PathBuf::from))
        .map_err(|message| CommandError {
            kind: "SearchLog",
            message,
        })
}

#[tauri::command]
async fn search_videos(
    request: SearchRequest,
    addon_search: Option<addons::AddonSearchLaunch>,
    app: tauri::AppHandle,
    engine: State<'_, Arc<SearchEngine>>,
    registry: State<'_, Arc<addons::AddonRegistry>>,
) -> Result<SearchPage, CommandError> {
    let engine = Arc::clone(engine.inner());
    let registry = Arc::clone(registry.inner());
    let page = tauri::async_runtime::spawn_blocking(move || match addon_search {
        Some(addon_search) => search_with_addon(request, &addon_search, &engine, &registry),
        None => engine.search(request).map_err(Into::into),
    })
    .await
    .map_err(|error| CommandError {
        kind: "Provider",
        message: format!("The search worker stopped unexpectedly: {error}"),
    })??;
    expose_cached_thumbnails(&app, &page)?;
    Ok(page)
}

/// Lets the webview load the thumbnails a page of results already carries.
/// Every command that hands out results has to do this: a thumbnail the
/// webview may not read stays blank, and because it has a path the frontend
/// never asks for it again.
fn expose_cached_thumbnails(app: &tauri::AppHandle, page: &SearchPage) -> Result<(), CommandError> {
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
    Ok(())
}

#[tauri::command]
async fn search_match_count(
    request: SearchRequest,
    addon_search: Option<addons::AddonSearchLaunch>,
    engine: State<'_, Arc<SearchEngine>>,
    registry: State<'_, Arc<addons::AddonRegistry>>,
) -> Result<usize, CommandError> {
    let engine = Arc::clone(engine.inner());
    let registry = Arc::clone(registry.inner());
    tauri::async_runtime::spawn_blocking(move || match addon_search {
        Some(addon_search) => match_count_with_addon(request, &addon_search, &engine, &registry),
        None => engine.match_count(request).map_err(Into::into),
    })
    .await
    .map_err(|error| CommandError {
        kind: "Provider",
        message: format!("The search worker stopped unexpectedly: {error}"),
    })?
}

/// The thumbnail for the result behind `result_id`: images are their
/// own thumbnail (the engine's fast path, kept byte-identical), anything
/// else is generated through `ts`. A failure reads exactly as a
/// generation failure always did.
fn thumbnail_path(engine: &SearchEngine, result_id: &str) -> Result<PathBuf, SearchError> {
    let path = engine.video_path(result_id)?;
    if search::is_supported_image(&path) {
        return Ok(path);
    }
    ts_client::thumbnail(&path).map_err(|_| SearchError::VideoUnavailable)
}

/// The preview frames for the result behind `result_id`: images are
/// their own single frame, anything else is generated through `ts`.
fn preview_paths(engine: &SearchEngine, result_id: &str) -> Result<Vec<PathBuf>, SearchError> {
    let path = engine.video_path(result_id)?;
    if search::is_supported_image(&path) {
        return Ok(vec![path]);
    }
    ts_client::preview(&path).map_err(|_| SearchError::VideoUnavailable)
}

#[tauri::command]
async fn video_thumbnail(
    result_id: String,
    app: tauri::AppHandle,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<String, CommandError> {
    // Thumbnail generation invokes ffmpeg (through `ts`). Keep it off the
    // command thread so a rapid scroll cannot freeze the WebView while a
    // tile is being filled.
    let engine = Arc::clone(engine.inner());
    let path = tauri::async_runtime::spawn_blocking(move || thumbnail_path(&engine, &result_id))
        .await
        .map_err(|error| CommandError {
            kind: "Thumbnail",
            message: format!("The thumbnail worker stopped unexpectedly: {error}"),
        })?
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
    // On a worker, not the main thread: this runs ffmpeg once per frame
    // (through `ts`), and on the main thread a first hover would freeze
    // the window for as long as that took.
    let engine = Arc::clone(engine.inner());
    let paths = tauri::async_runtime::spawn_blocking(move || preview_paths(&engine, &result_id))
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

/// Moves a video to the trash and returns what undo needs to find it again:
/// the item's name inside the Linux trash, or its trashed path on macOS. Both
/// are looked up after the move, because the trash renames an item whose name
/// is already taken, so the file's own name is not enough to identify it.
fn move_to_trash(path: &std::path::Path) -> Result<Option<String>, CommandError> {
    #[cfg(not(target_os = "macos"))]
    let before = trash_listing();
    #[cfg(target_os = "linux")]
    let output = std::process::Command::new("gio")
        .args(["trash", path.to_string_lossy().as_ref()])
        .output();
    #[cfg(target_os = "macos")]
    let output = std::process::Command::new("osascript")
        .args(["-e", MACOS_TRASH_SCRIPT, "--"])
        .arg(path)
        .output();
    let output = output.map_err(|error| CommandError {
        kind: "Delete",
        message: format!("The video could not be moved to the trash: {error}"),
    })?;
    let name = trash_outcome(
        path,
        output.status.success(),
        &String::from_utf8_lossy(&output.stderr),
        // Checked after the helper has run, and deliberately not through
        // `exists`: a symlink that still dangles was not trashed either.
        path.symlink_metadata().is_ok(),
    )?;
    #[cfg(target_os = "macos")]
    {
        let trashed = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok(Some(if trashed.is_empty() { name } else { trashed }))
    }
    #[cfg(not(target_os = "macos"))]
    {
        // The item this call added is the one whose name was not there before,
        // whatever the trash renamed it to and whatever the clock says.
        // An ambiguous listing leaves the identity unknown: undo is then not
        // offered, rather than risking restoring an older item of the same name.
        let _ = name;
        Ok(new_trash_item(&before, &trash_listing()))
    }
}

/// The names of everything in the Linux trash, as URIs (`gio list -u`).
#[cfg(not(target_os = "macos"))]
fn trash_listing() -> String {
    std::process::Command::new("gio")
        .args(["list", "--hidden", "-u", "trash:///"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

/// The AppleScript run by `move_to_trash`. The path arrives as an argument, so
/// no file name can alter the script; it returns where the item now lives.
#[cfg(target_os = "macos")]
const MACOS_TRASH_SCRIPT: &str = "on run argv\n\
    tell application \"Finder\"\n\
    set trashed to delete (POSIX file (item 1 of argv))\n\
    return POSIX path of (trashed as alias)\n\
    end tell\n\
    end run";

/// The one item in `after` that was not in `before`: two listings of the
/// trash, one name per line. Anything other than exactly one new item is
/// ambiguous (something else was trashed meanwhile), so it is not guessed at.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn new_trash_item(before: &str, after: &str) -> Option<String> {
    let known: std::collections::HashSet<&str> = before.lines().map(name_column).collect();
    let mut added = after
        .lines()
        .map(name_column)
        .filter(|uri| !known.contains(uri));
    let item = added.next()?;
    if added.next().is_some() {
        return None;
    }
    item.strip_prefix("trash:///").map(str::to_owned)
}

fn name_column(line: &str) -> &str {
    line.split('\t').next().unwrap_or(line)
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

/// What Toka was launched with, ready for the normal player or search flow.
#[tauri::command]
async fn launch_intent(
    launched: State<'_, LaunchRequest>,
    app: tauri::AppHandle,
    engine: State<'_, Arc<SearchEngine>>,
    registry: State<'_, Arc<addons::AddonRegistry>>,
) -> Result<Option<LaunchResponse>, CommandError> {
    let intent = launched.0.clone().map_err(|message| CommandError {
        kind: "Launch",
        message,
    })?;
    let engine = Arc::clone(engine.inner());
    let registry = Arc::clone(registry.inner());
    let response = tauri::async_runtime::spawn_blocking(move || {
        let response = match intent {
            launch::LaunchIntent::Empty => return Ok(None),
            launch::LaunchIntent::Media(path) => LaunchResponse {
                page: launch_page(&path, &engine)?,
                addon_search: None,
            },
            launch::LaunchIntent::AddonSearch(addon_search) => LaunchResponse {
                page: addon_search_page(addon_search.clone(), &engine, &registry)?,
                addon_search: Some(addon_search),
            },
        };
        Ok::<_, CommandError>(Some(response))
    })
    .await
    .map_err(|error| CommandError {
        kind: "Launch",
        message: format!("The launch worker stopped unexpectedly: {error}"),
    })??;
    if let Some(response) = &response {
        expose_cached_thumbnails(&app, &response.page)?;
    }
    Ok(response)
}

fn launch_page(path: &std::path::Path, engine: &SearchEngine) -> Result<SearchPage, CommandError> {
    if path.is_dir() {
        return directory_page(path, engine);
    }
    if search::is_supported_media(path, search::MediaType::Both) {
        return single_file_page(path, engine);
    }
    let videos = playlist::videos(path).map_err(|message| CommandError {
        kind: "Playlist",
        message,
    })?;
    // No query: a playlist's entries come from its lines rather than a
    // search, and only folder and file launches may speak `path:` — the
    // frontend tells them apart by exactly that.
    Ok(engine.page_of_videos(String::new(), videos))
}

/// A page over a folder's own media, named by the `path:` query that finds
/// the same files again: launching with a folder and searching for it are
/// the same question.
fn directory_page(
    path: &std::path::Path,
    engine: &SearchEngine,
) -> Result<SearchPage, CommandError> {
    let mut videos = playlist::collect_directory_media(path).map_err(|message| CommandError {
        kind: "Playlist",
        message,
    })?;
    playlist::shuffle_paths(&mut videos);
    Ok(engine.page_of_videos(query_for_location(path), videos))
}

/// A page over the one file, named the same way.
fn single_file_page(
    path: &std::path::Path,
    engine: &SearchEngine,
) -> Result<SearchPage, CommandError> {
    if path.is_dir() {
        return Err(CommandError {
            kind: "Playlist",
            message: format!(
                "{} is a folder. Open it as a folder to play everything inside.",
                playlist::name(path)
            ),
        });
    }
    if !path.is_file() {
        return Err(CommandError {
            kind: "Playlist",
            message: format!("{} could not be found.", playlist::name(path)),
        });
    }
    Ok(engine.page_of_videos(query_for_location(path), vec![path.to_path_buf()]))
}

/// The `path:` query the search field shows for an opened location:
/// canonical, so a relative launch still names files the search finds after
/// the working directory has moved on.
fn query_for_location(path: &std::path::Path) -> String {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    search::path_query_for(&absolute)
}

fn check_openable_file(path: &std::path::Path) -> Result<(), CommandError> {
    if search::is_supported_media(path, search::MediaType::Both) {
        return Ok(());
    }
    Err(CommandError {
        kind: "Playlist",
        message: format!(
            "{} is not a video or image Toka can play.",
            playlist::name(path)
        ),
    })
}

/// Plays a folder picked from the file browser: the same page a
/// `toka /that/folder` launch opens, under the same `path:` query.
#[tauri::command]
async fn open_folder(
    path: String,
    app: tauri::AppHandle,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<SearchPage, CommandError> {
    let engine = Arc::clone(engine.inner());
    let page =
        tauri::async_runtime::spawn_blocking(move || directory_page(Path::new(&path), &engine))
            .await
            .map_err(|error| CommandError {
                kind: "Playlist",
                message: format!("The folder worker stopped unexpectedly: {error}"),
            })??;
    expose_cached_thumbnails(&app, &page)?;
    Ok(page)
}

/// Plays one file picked from the file browser, named by its own `path:`
/// query the way a `toka /that/video.mp4` launch is.
#[tauri::command]
async fn open_file(
    path: String,
    app: tauri::AppHandle,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<SearchPage, CommandError> {
    check_openable_file(Path::new(&path))?;
    let engine = Arc::clone(engine.inner());
    let page =
        tauri::async_runtime::spawn_blocking(move || single_file_page(Path::new(&path), &engine))
            .await
            .map_err(|error| CommandError {
                kind: "Playlist",
                message: format!("The file worker stopped unexpectedly: {error}"),
            })??;
    expose_cached_thumbnails(&app, &page)?;
    Ok(page)
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

struct SystemActionHost {
    app: tauri::AppHandle,
    executable: PathBuf,
}

impl addons::ActionHost for SystemActionHost {
    fn open_related_search(&mut self, search: addons::AddonSearchLaunch) -> Result<(), String> {
        let encoded = serde_json::to_string(&search)
            .map_err(|error| format!("The related search could not be prepared: {error}"))?;
        let child = std::process::Command::new(&self.executable)
            .arg(launch::ADDON_SEARCH_FLAG)
            .arg(encoded)
            .spawn()
            .map_err(|error| format!("Another Toka could not be started: {error}"))?;
        reap_child(child);
        Ok(())
    }

    fn open_url(&mut self, url: &url::Url) -> Result<(), String> {
        self.app
            .opener()
            .open_url(url.as_str(), None::<&str>)
            .map_err(|error| format!("{} could not be opened: {error}", url.as_str()))
    }
}

#[tauri::command]
fn matching_addon_actions(
    result_id: String,
    engine: State<'_, Arc<SearchEngine>>,
    registry: State<'_, Arc<addons::AddonRegistry>>,
) -> Result<addons::AddonMenu, CommandError> {
    let path = engine.video_path(&result_id).map_err(CommandError::from)?;
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    Ok(registry.matching_actions(&file_name))
}

#[tauri::command]
fn run_addon_action(
    result_id: String,
    addon_id: String,
    action_id: String,
    app: tauri::AppHandle,
    engine: State<'_, Arc<SearchEngine>>,
    registry: State<'_, Arc<addons::AddonRegistry>>,
) -> Result<(), CommandError> {
    let path = engine.video_path(&result_id).map_err(CommandError::from)?;
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let executable = std::env::current_exe().map_err(|error| CommandError {
        kind: "Addon",
        message: format!("Toka could not find its own program: {error}"),
    })?;
    let mut host = SystemActionHost { app, executable };
    registry
        .run_action(&file_name, &addon_id, &action_id, &mut host)
        .map_err(addon_error)
}

/// Starts another Toka. The desktop launcher raises the one already running
/// rather than opening a second, so the running one has to be able to ask.
#[tauri::command]
fn open_new_window() -> Result<(), CommandError> {
    start_toka(None)
}

/// Starts another Toka on the folder the video `result_id` names sits in, the
/// way `toka /that/folder` would, so it plays the rest of that folder while
/// this one carries on with its own playlist.
#[tauri::command]
fn open_folder_in_new_window(
    result_id: String,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<(), CommandError> {
    let path = engine.video_path(&result_id).map_err(CommandError::from)?;
    start_toka(Some(folder_of(&path)?))
}

#[tauri::command]
fn open_parent_folder_in_new_window(
    result_id: String,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<(), CommandError> {
    let path = engine.video_path(&result_id).map_err(CommandError::from)?;
    start_toka(Some(&parent_folder_of(&path)?))
}

/// The folder above the folder the video sits in, for when the folders around
/// it are what the viewer wants to browse next.
fn parent_folder_of(path: &std::path::Path) -> Result<std::path::PathBuf, CommandError> {
    // Launches keep the path as given, so a relative launch stays relative:
    // resolve it first, or the parent of a relative folder is wrongly empty.
    let absolute = to_absolute(path);
    // Canonical when possible, so `..` resolves through symlinks exactly as
    // the filesystem does; lexical otherwise, so a video that moved since it
    // was indexed can still open somewhere sensible.
    let resolved = std::fs::canonicalize(&absolute).unwrap_or_else(|_| absolute_lexical(&absolute));
    let folder = folder_of(&resolved)?;
    folder
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| CommandError {
            kind: "NewWindow",
            message: format!("{} has no parent folder Toka can open.", path.display()),
        })
}

/// The path made absolute without resolving `.` or `..`, so canonicalization
/// below still sees symlinks before any parent segment.
fn to_absolute(path: &std::path::Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    }
}

/// An absolute path with `.` and `..` resolved lexically, without touching
/// the disk (which may have moved on since the video was indexed).
fn absolute_lexical(absolute: &std::path::Path) -> std::path::PathBuf {
    let mut normalized = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn folder_of(path: &std::path::Path) -> Result<&std::path::Path, CommandError> {
    path.parent()
        .filter(|folder| !folder.as_os_str().is_empty())
        .ok_or_else(|| CommandError {
            kind: "NewWindow",
            message: format!("{} is not inside a folder Toka can open.", path.display()),
        })
}

fn reap_child(mut child: std::process::Child) {
    drop(std::thread::spawn(move || {
        let _ = child.wait();
    }));
}

fn start_toka(folder: Option<&std::path::Path>) -> Result<(), CommandError> {
    let executable = std::env::current_exe().map_err(|error| CommandError {
        kind: "NewWindow",
        message: format!("Toka could not find its own program to start again: {error}"),
    })?;
    let mut command = std::process::Command::new(&executable);
    if let Some(folder) = folder {
        command.arg(folder);
    }
    let child = command.spawn().map_err(|error| CommandError {
        kind: "NewWindow",
        message: format!("Another Toka could not be started: {error}"),
    })?;
    reap_child(child);
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
    ts_client::sync_path_best_effort(&path);
    *deleted.lock().unwrap() = trash_name.map(|trash_name| DeletedVideo {
        trash_name,
        original_path: path,
    });
    Ok(())
}

/// Puts a trashed video back where it came from. Never overwrites: if something
/// has taken the original path since, the restore is reported as failed.
#[cfg(target_os = "macos")]
fn restore_from_trash(item: &DeletedVideo) -> std::io::Result<bool> {
    if !Path::new(&item.trash_name).is_absolute() || item.original_path.symlink_metadata().is_ok() {
        return Ok(false);
    }
    // The Trash is protected, so a plain move can be refused; Finder is allowed.
    if tags::move_without_replacing(std::path::Path::new(&item.trash_name), &item.original_path)
        .is_ok()
    {
        return Ok(true);
    }
    let Some(folder) = item.original_path.parent() else {
        return Ok(false);
    };
    let moved = std::process::Command::new("osascript")
        .args([
            "-e",
            "on run argv\ntell application \"Finder\" to move (POSIX file (item 1 of argv) as alias) to (POSIX file (item 2 of argv) as alias)\nend run",
            "--",
        ])
        .arg(&item.trash_name)
        .arg(folder)
        .output()?
        .status
        .success();
    Ok(moved)
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
        .output()
        .map(|out| out.status.success());
    #[cfg(target_os = "macos")]
    let output = restore_from_trash(&item);
    let output = output.map_err(|error| CommandError {
        kind: "Delete",
        message: format!("The deleted video could not be restored: {error}"),
    })?;
    if output {
        ts_client::sync_path_best_effort(&item.original_path);
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
    update_video_tags(
        &result_id,
        &tags,
        &engine,
        tags::set,
        ts_client::sync_rename_best_effort,
    )
}

#[tauri::command]
fn add_video_tags(
    result_id: String,
    tags: Vec<String>,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<VideoTagUpdate, TagsError> {
    update_video_tags(
        &result_id,
        &tags,
        &engine,
        tags::add,
        ts_client::sync_rename_best_effort,
    )
}

#[tauri::command]
fn remove_video_tags(
    result_id: String,
    tags: Vec<String>,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<VideoTagUpdate, TagsError> {
    update_video_tags(
        &result_id,
        &tags,
        &engine,
        tags::remove,
        ts_client::sync_rename_best_effort,
    )
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
    tag_videos(
        &result_ids,
        &tags,
        &engine,
        ts_client::sync_rename_best_effort,
    )
}

fn tag_videos(
    result_ids: &[String],
    tags: &[String],
    engine: &SearchEngine,
    on_rename: impl Fn(&Path, &Path),
) -> BulkTagUpdate {
    let mut tagged = Vec::new();
    let mut failed = 0;
    let mut problem = None;
    for result_id in result_ids {
        // One video that has moved or been deleted since the search is a
        // reason to say so afterwards, not a reason to abandon the rest of
        // the page half tagged.
        match update_video_tags(result_id, tags, engine, tags::add, &on_rename) {
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

/// Retags the video behind `result_id` and keeps the index pointing at
/// the renamed file before the command returns.
fn update_video_tags(
    result_id: &str,
    tags: &[String],
    engine: &SearchEngine,
    operation: fn(&Path, &[String]) -> Result<tags::TagUpdate, String>,
    on_rename: impl Fn(&Path, &Path),
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
    on_rename(&path, &update.path);
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
async fn get_top_tags() -> Result<Vec<toka_search::top_tags::TopTag>, CommandError> {
    let failed = |message: String| CommandError {
        kind: "Tags",
        message,
    };
    let database = toka_search::owned_index::database_path().map_err(failed)?;
    // The indexer daemon keeps this list fresh; count on demand until it has.
    // An empty list may predate the first scan, so it is counted again.
    // A stale list (daemon stopped, or the app owns the index) is counted
    // again. An empty one may predate the first scan, so it holds for a minute.
    let minutes = |minutes: u64| std::time::Duration::from_secs(minutes * 60);
    if let Some(tags) = toka_search::top_tags::read_fresh(&database, minutes(10)) {
        if !tags.is_empty() || toka_search::top_tags::read_fresh(&database, minutes(1)).is_some() {
            return Ok(tags);
        }
    }
    if !database.exists() {
        return Ok(Vec::new());
    }
    // A full count reads every file's tags: off the async runtime's threads.
    tauri::async_runtime::spawn_blocking(move || toka_search::top_tags::refresh(&database))
        .await
        .map_err(|error| failed(error.to_string()))?
        .map_err(failed)
}

#[tauri::command]
fn add_folder_tags(
    result_id: String,
    tags: Vec<String>,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<FolderTagUpdate, TagsError> {
    update_folder_tags(
        &result_id,
        &tags,
        &engine,
        tags::add,
        ts_client::sync_rename_best_effort,
    )
}

#[tauri::command]
fn remove_folder_tags(
    result_id: String,
    tags: Vec<String>,
    engine: State<'_, Arc<SearchEngine>>,
) -> Result<FolderTagUpdate, TagsError> {
    update_folder_tags(
        &result_id,
        &tags,
        &engine,
        tags::remove,
        ts_client::sync_rename_best_effort,
    )
}

/// Retags the folder the video behind `result_id` sits in. That renames the
/// folder, so the index follows every result inside it.
fn update_folder_tags(
    result_id: &str,
    tags: &[String],
    engine: &SearchEngine,
    operation: fn(&Path, &[String]) -> Result<tags::TagUpdate, String>,
    on_rename: impl Fn(&Path, &Path),
) -> Result<FolderTagUpdate, TagsError> {
    let path = engine.video_path(result_id).map_err(|error| TagsError {
        kind: "VideoUnavailable",
        message: error.to_string(),
    })?;
    // A video at the top of the filesystem has no folder a name can carry.
    let folder = path
        .parent()
        .filter(|folder| folder.parent().is_some())
        .ok_or_else(|| TagsError {
            kind: "Tags",
            message: "That video's folder cannot be tagged.".into(),
        })?;
    let update = operation(folder, tags).map_err(|message| TagsError {
        kind: "Tags",
        message,
    })?;
    let mut result_ids = engine.rename_folder(folder, update.path.clone());
    on_rename(folder, &update.path);
    result_ids.sort();
    Ok(FolderTagUpdate {
        folder_tags: update.tags,
        result_ids,
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

fn platform_provider(_logger: Arc<SearchLogger>) -> Result<Arc<dyn SearchProvider>, String> {
    // Every macOS and Linux build searches through `ts` — integration
    // builds too, with their fixture videos forwarded to the `ts`
    // children. The legacy TOKA_SEARCH_PROVIDER override selected real
    // providers that no longer exist; it must not fall through to a
    // persistent index here.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        Ok(Arc::new(ts_client::TsSearchProvider::new()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        struct UnsupportedProvider;
        impl SearchProvider for UnsupportedProvider {
            fn candidates(
                &self,
                _query: &str,
                _whole_path: bool,
                _context: &search_log::SearchLogContext,
            ) -> Result<Vec<std::path::PathBuf>, SearchError> {
                Err(SearchError::Provider(
                    "Toka currently supports video search on macOS and Linux.".into(),
                ))
            }
        }
        Ok(Arc::new(UnsupportedProvider))
    }
}

fn addon_directory(
    config_directory: Option<PathBuf>,
    e2e_directory: Option<std::ffi::OsString>,
    e2e: bool,
) -> Option<PathBuf> {
    if e2e {
        e2e_directory.map(PathBuf::from)
    } else {
        config_directory.map(|directory| directory.join("app.toka.desktop").join("addons"))
    }
}

/// Installs the indexer service idempotently at startup, so the first
/// launch after upgrade works with no user action. Best effort: when
/// `ts` is missing or the service manager refuses, the app logs a
/// warning and runs degraded — reads work, only freshness needs the
/// daemon. Skipped in e2e builds, which must never touch real services.
#[cfg(all(any(target_os = "linux", target_os = "macos"), not(feature = "e2e")))]
fn ensure_search_service() {
    match ts_client::ensure_installed() {
        Ok(report) => {
            // Changed steps are worth one line each — the first launch
            // after upgrade says what it set up — while the steady
            // state's already-done steps stay silent.
            for action in report.actions {
                if action.changed {
                    eprintln!("Toka indexer: {}", action.message);
                }
            }
            if let Some(error) = report.error {
                eprintln!("Toka could not install its indexer service: {error}");
            }
        }
        Err(error) => eprintln!("Toka could not install its indexer service: {error}"),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    // `--indexer` is gone (the indexer is `ts daemon run` now): route it
    // to the CLI for a clear error, never to a GUI launch. It stays out
    // of `is_command` so restarts still stop a stale pre-`ts` indexer
    // instead of skipping it as another CLI command.
    #[cfg(not(mobile))]
    if cli::is_command(&arguments)
        || arguments
            .get(1)
            .is_some_and(|argument| argument == std::ffi::OsStr::new("--indexer"))
    {
        let status = {
            let mut stdout = std::io::stdout();
            let mut stderr = std::io::stderr();
            cli::run_command(&arguments, ts_client::run_cli, &mut stdout, &mut stderr)
        };
        if status != 0 {
            std::process::exit(status);
        }
        return;
    }

    // A release GUI launch from an interactive shell hands off to a detached
    // child with no standard streams and exits, so the shell gets its prompt
    // back immediately and never sees the GUI toolkit's output. CLI commands,
    // debug builds such as `tauri dev`, and piped launches stay in this
    // process.
    #[cfg(all(target_os = "linux", not(feature = "e2e"), not(debug_assertions)))]
    if crate::launch::should_detach_gui(
        &arguments,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        std::io::stderr().is_terminal(),
    ) {
        crate::launch::detach_gui(&arguments);
    }

    #[cfg(all(any(target_os = "linux", target_os = "macos"), not(feature = "e2e")))]
    ensure_search_service();
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init());
    // The owned-index run automates the real app, so it needs the
    // WebDriver server without the e2e fixtures (see Cargo.toml).
    #[cfg(any(feature = "e2e", feature = "webdriver"))]
    let builder = builder
        .plugin(tauri_plugin_wdio::init())
        .plugin(tauri_plugin_wdio_webdriver::init());

    // Read before the window exists, because the command line Toka was started
    // with is the whole of what it was asked to open.
    let launched = LaunchRequest(launch::from_arguments(arguments));
    let user_addons = addon_directory(
        dirs::config_dir(),
        std::env::var_os("TOKA_E2E_ADDONS_DIR"),
        cfg!(feature = "e2e"),
    );
    let registry = Arc::new(addons::AddonRegistry::system(user_addons.as_deref()));
    let logger = Arc::new(SearchLogger::system());
    // No rename overlay of the app's own: the overlay lives with the
    // index, so only `ts` reads and writes it. The engine keeps renames
    // in memory for this session; the index learns them through syncs.
    let engine = Arc::new(SearchEngine::new_with_logger(
        platform_provider(logger.clone()).expect("Toka could not initialize its search provider"),
        logger.clone(),
    ));
    let builder = builder
        .manage(logger.clone())
        .manage(engine)
        .manage(registry)
        .manage(Mutex::new(None::<DeletedVideo>))
        .manage(launched);
    #[cfg(all(target_os = "linux", feature = "e2e", not(feature = "native-e2e")))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        search_videos,
        search_match_count,
        search_log_settings,
        set_search_log_settings,
        index_state,
        launch_intent,
        open_folder,
        open_file,
        video_thumbnail,
        video_preview,
        delete_video,
        undo_delete,
        external_players,
        open_in_external_player,
        open_new_window,
        open_folder_in_new_window,
        open_parent_folder_in_new_window,
        matching_addon_actions,
        run_addon_action,
        set_video_tags,
        add_video_tags,
        remove_video_tags,
        add_tags_to_videos,
        add_folder_tags,
        remove_folder_tags,
        get_top_tags,
        prepare_video,
        subtitle_cues
    ]);
    #[cfg(all(target_os = "linux", any(not(feature = "e2e"), feature = "native-e2e")))]
    let builder = {
        let player = player_linux::NativePlayer::new();
        let setup_player = player.clone();
        builder
            .manage(player)
            .setup(move |app| player_linux::install(app, setup_player.clone()))
            .invoke_handler(tauri::generate_handler![
                search_videos,
                search_match_count,
                search_log_settings,
                set_search_log_settings,
                index_state,
                add_index_folder,
                remove_index_folder,
                launch_intent,
                open_folder,
                open_file,
                video_thumbnail,
                video_preview,
                delete_video,
                undo_delete,
                external_players,
                open_in_external_player,
                open_new_window,
                open_folder_in_new_window,
                open_parent_folder_in_new_window,
                matching_addon_actions,
                run_addon_action,
                set_video_tags,
                add_video_tags,
                remove_video_tags,
                add_tags_to_videos,
                add_folder_tags,
                remove_folder_tags,
                get_top_tags,
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
    #[cfg(all(target_os = "macos", not(feature = "e2e")))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        search_videos,
        search_match_count,
        search_log_settings,
        set_search_log_settings,
        index_state,
        add_index_folder,
        remove_index_folder,
        launch_intent,
        open_folder,
        open_file,
        video_thumbnail,
        video_preview,
        delete_video,
        undo_delete,
        external_players,
        open_in_external_player,
        open_new_window,
        open_folder_in_new_window,
        open_parent_folder_in_new_window,
        matching_addon_actions,
        run_addon_action,
        set_video_tags,
        add_video_tags,
        remove_video_tags,
        add_tags_to_videos,
        add_folder_tags,
        remove_folder_tags,
        get_top_tags,
        prepare_video,
        subtitle_cues
    ]);
    #[cfg(all(target_os = "macos", feature = "e2e"))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        search_videos,
        search_match_count,
        search_log_settings,
        set_search_log_settings,
        index_state,
        launch_intent,
        open_folder,
        open_file,
        video_thumbnail,
        video_preview,
        delete_video,
        undo_delete,
        external_players,
        open_in_external_player,
        open_new_window,
        open_folder_in_new_window,
        open_parent_folder_in_new_window,
        matching_addon_actions,
        run_addon_action,
        set_video_tags,
        add_video_tags,
        remove_video_tags,
        add_tags_to_videos,
        add_folder_tags,
        remove_folder_tags,
        get_top_tags,
        prepare_video,
        subtitle_cues
    ]);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        search_videos,
        search_match_count,
        search_log_settings,
        set_search_log_settings,
        index_state,
        launch_intent,
        open_folder,
        open_file,
        video_thumbnail,
        video_preview,
        delete_video,
        undo_delete,
        external_players,
        open_in_external_player,
        open_new_window,
        open_folder_in_new_window,
        open_parent_folder_in_new_window,
        matching_addon_actions,
        run_addon_action,
        set_video_tags,
        add_video_tags,
        remove_video_tags,
        add_tags_to_videos,
        add_folder_tags,
        remove_folder_tags,
        get_top_tags,
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
    use toka_search::search_log::SearchLogContext;

    #[test]
    fn addon_launch_page_keeps_only_the_exact_creator() {
        let directory = tempfile::tempdir().unwrap();
        let exact = directory
            .path()
            .join("2026-10-10_@sam_18938383490812348343.mp4");
        let similar = directory
            .path()
            .join("2026-10-10_@sammy_18938383490812348344.mp4");
        std::fs::write(&exact, b"test").unwrap();
        std::fs::write(&similar, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider(vec![exact, similar])));
        let registry = addons::AddonRegistry::system(None);
        let search = addons::AddonSearchLaunch {
            query: "@sam".into(),
            fields: search::SearchFields {
                tags: false,
                file_name: true,
                path: false,
            },
            media_type: search::MediaType::Videos,
            addon_id: "tiktok".into(),
            capture: "handle".into(),
            value: "@sam".into(),
        };

        let page = addon_search_page(search, &engine, &registry).unwrap();

        assert_eq!(page.total_results, 1);
        assert_eq!(
            page.results[0].file_name,
            "2026-10-10_@sam_18938383490812348343.mp4"
        );
    }

    #[test]
    fn a_launched_folder_is_named_by_the_path_query_that_finds_it_again() {
        let directory = tempfile::tempdir().unwrap();
        let shoots = directory.path().join("shoots");
        std::fs::create_dir(&shoots).unwrap();
        std::fs::write(shoots.join("clip.mp4"), b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider(Vec::new())));

        let page = launch_page(&shoots, &engine).unwrap();

        let canonical = std::fs::canonicalize(&shoots).unwrap();
        assert_eq!(page.query, format!("path:{}", canonical.display()));
        assert_eq!(page.total_results, 1);
    }

    #[test]
    fn a_launched_folder_with_spaces_is_quoted_in_its_path_query() {
        let directory = tempfile::tempdir().unwrap();
        let shoots = directory.path().join("my shoots");
        std::fs::create_dir(&shoots).unwrap();
        std::fs::write(shoots.join("clip.mp4"), b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider(Vec::new())));

        let page = launch_page(&shoots, &engine).unwrap();

        let canonical = std::fs::canonicalize(&shoots).unwrap();
        assert_eq!(page.query, format!("path:\"{}\"", canonical.display()));
    }

    #[test]
    fn a_launched_video_is_named_by_its_own_path_query() {
        let directory = tempfile::tempdir().unwrap();
        let clip = directory.path().join("clip.mp4");
        std::fs::write(&clip, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider(Vec::new())));

        let page = launch_page(&clip, &engine).unwrap();

        let canonical = std::fs::canonicalize(&clip).unwrap();
        assert_eq!(page.query, format!("path:{}", canonical.display()));
        assert_eq!(page.total_results, 1);
    }

    #[test]
    fn a_launched_image_is_named_by_its_own_path_query() {
        let directory = tempfile::tempdir().unwrap();
        let photo = directory.path().join("trip.jpg");
        std::fs::write(&photo, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider(Vec::new())));

        let page = launch_page(&photo, &engine).unwrap();

        let canonical = std::fs::canonicalize(&photo).unwrap();
        assert_eq!(page.query, format!("path:{}", canonical.display()));
        assert_eq!(page.total_results, 1);
    }

    #[test]
    fn a_folder_opened_as_a_file_says_so() {
        let directory = tempfile::tempdir().unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider(Vec::new())));

        let error = single_file_page(directory.path(), &engine).unwrap_err();

        assert_eq!(error.kind, "Playlist");
        assert!(error.message.contains("is a folder"), "{}", error.message);
    }

    #[test]
    fn a_launched_playlist_carries_no_query_to_mistake_for_a_path() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("beach.mp4"), b"test").unwrap();
        let playlist = directory.path().join("summer.m3u8");
        std::fs::write(&playlist, "beach.mp4\n").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider(Vec::new())));

        let page = launch_page(&playlist, &engine).unwrap();

        assert_eq!(page.query, "");
        assert_eq!(page.total_results, 1);
    }

    #[test]
    fn an_opened_file_may_be_a_video_or_an_image_but_nothing_else() {
        assert!(check_openable_file(Path::new("/Videos/clip.mp4")).is_ok());
        assert!(check_openable_file(Path::new("/Photos/trip.JPG")).is_ok());

        let error = check_openable_file(Path::new("/Videos/notes.txt")).unwrap_err();

        assert_eq!(error.kind, "Playlist");
        assert!(error.message.contains("notes.txt"), "{}", error.message);

        // Playlists stay a launch-only way to play: the file browser opens
        // one file.
        let error = check_openable_file(Path::new("/Videos/summer.m3u8")).unwrap_err();

        assert_eq!(error.kind, "Playlist");
    }

    #[test]
    fn e2e_addons_ignore_the_real_user_config_directory() {
        let real_config = Some(PathBuf::from("/real/config"));

        assert_eq!(
            addon_directory(real_config.clone(), None, false),
            Some(PathBuf::from("/real/config/app.toka.desktop/addons"))
        );
        assert_eq!(addon_directory(real_config.clone(), None, true), None);
        assert_eq!(
            addon_directory(
                real_config,
                Some(std::ffi::OsString::from("/test/addons")),
                true
            ),
            Some(PathBuf::from("/test/addons"))
        );
    }

    #[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "e2e"))]
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
    fn the_item_a_delete_added_to_the_trash_is_the_one_restored() {
        let before = "trash:///clip.mp4\t2\t(regular)\ntrash:///clip.mp4.9\t2\t(regular)\n";
        let after = "trash:///clip.mp4\t2\t(regular)\ntrash:///clip.mp4.9\t2\t(regular)\ntrash:///clip.mp4.10\t2\t(regular)\n";

        assert_eq!(
            new_trash_item(before, after).as_deref(),
            Some("clip.mp4.10")
        );
        assert_eq!(new_trash_item(after, after), None);
        assert_eq!(new_trash_item("", after), None);
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
    fn a_videos_folder_is_the_folder_it_sits_in() {
        assert_eq!(
            folder_of(Path::new("/root/path/to/my/video.mp4")).unwrap(),
            Path::new("/root/path/to/my")
        );
    }

    #[test]
    fn a_path_with_no_folder_says_so() {
        let error = folder_of(Path::new("video.mp4")).unwrap_err();

        assert_eq!(error.kind, "NewWindow");
    }

    #[test]
    fn a_videos_parent_folder_is_the_folder_above_the_one_it_sits_in() {
        assert_eq!(
            parent_folder_of(Path::new("/root/path/to/my/video.mp4")).unwrap(),
            Path::new("/root/path/to")
        );
    }

    #[test]
    fn a_video_directly_under_the_root_has_no_parent_folder_to_open() {
        let error = parent_folder_of(Path::new("/video.mp4")).unwrap_err();

        assert_eq!(error.kind, "NewWindow");
    }

    #[test]
    fn a_relative_video_resolves_against_the_current_directory() {
        assert_eq!(
            parent_folder_of(Path::new("clips/video.mp4")).unwrap(),
            std::env::current_dir().unwrap()
        );
    }

    #[test]
    fn a_bare_filename_sits_in_the_current_directory() {
        let current = std::env::current_dir().unwrap();

        assert_eq!(
            parent_folder_of(Path::new("video.mp4")).unwrap(),
            current.parent().unwrap()
        );
    }

    #[test]
    fn a_dot_dot_segment_does_not_become_the_parent() {
        assert_eq!(
            parent_folder_of(Path::new("/Playlists/Summer/../party.mkv")).unwrap(),
            Path::new("/")
        );
    }

    #[test]
    fn a_moved_video_falls_back_to_the_lexical_parent() {
        assert_eq!(
            parent_folder_of(Path::new("/no/such/dir/video.mp4")).unwrap(),
            Path::new("/no/such")
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_dot_dot_through_a_symlink_follows_the_filesystem() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("library").join("real");
        std::fs::create_dir_all(&target).unwrap();
        symlink(&target, root.path().join("link")).unwrap();
        // The file exists, as a playing video does when the action runs.
        std::fs::write(root.path().join("library").join("clip.mp4"), b"x").unwrap();

        // Physically `link/..` is `library`, not the root the link sits in.
        let video = root.path().join("link").join("..").join("clip.mp4");
        let expected = std::fs::canonicalize(root.path()).unwrap();

        assert_eq!(parent_folder_of(&video).unwrap(), expected);
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

    /// The app's startup provider searches through `ts`: one search
    /// round-trips candidates, visibility roots, and the revision.
    #[cfg(all(unix, any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn startup_provider_searches_through_ts() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("fixtures");
        std::fs::create_dir(&root).unwrap();
        let sample = root.join("sample.mp4");
        std::fs::write(&sample, b"sample video").unwrap();
        let candidates = serde_json::json!({
            "schema": 1,
            "query": "sample",
            "indexRevision": 42,
            "candidates": [sample.to_string_lossy()],
        });
        let folders = serde_json::json!({
            "schema": 1,
            "folders": [{
                "id": "aaa",
                "path": root.to_string_lossy(),
                "status": "ready",
                "message": null,
                "scannedFiles": 1,
                "currentPath": null,
                "recentDirectories": [],
            }],
        });
        let shim = directory.path().join("ts");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\nif [ \"$1\" = candidates ]; then cat <<'TS_EOF'\n{candidates}\nTS_EOF\nelif [ \"$1\" = folders ]; then cat <<'TS_EOF'\n{folders}\nTS_EOF\nelse echo \"unexpected argv: $*\" >&2; exit 9; fi\n"
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = ts_client::test_env::EnvGuard::lock();
        env.set("TS_BIN", shim.as_os_str());
        env.remove("TOKA_E2E_VIDEOS");

        let logger = Arc::new(SearchLogger::disabled());
        let provider = platform_provider(logger.clone()).unwrap();
        let engine = SearchEngine::new_with_logger(provider, logger);
        let page = engine
            .search(SearchRequest {
                query: "sample".into(),
                page: 1,
                page_size: search::PAGE_SIZE,
                search_id: None,
                fields: Default::default(),
                media_type: search::MediaType::Videos,
                sort: search::SearchSort::default(),
            })
            .unwrap();

        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "sample.mp4");
        assert_eq!(page.index_revision, 42);
    }

    struct FakeProvider(Vec<PathBuf>);

    impl SearchProvider for FakeProvider {
        fn candidates(
            &self,
            _query: &str,
            _whole_path: bool,
            _context: &SearchLogContext,
        ) -> Result<Vec<PathBuf>, SearchError> {
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
                search_id: None,
                fields: Default::default(),
                media_type: Default::default(),
                sort: search::SearchSort::default(),
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
        let renamed = Arc::new(Mutex::new(Vec::new()));
        let record = {
            let renamed = Arc::clone(&renamed);
            move |from: &Path, to: &Path| {
                renamed
                    .lock()
                    .unwrap()
                    .push((from.to_path_buf(), to.to_path_buf()));
            }
        };

        let update = tag_videos(&ids, &["beach".to_owned()], &engine, record);

        assert_eq!(update.failed, 0);
        assert_eq!(update.problem, None);
        assert_eq!(update.tagged.len(), 2);
        // Every tag syncs its rename to the index: production passes the
        // `ts` sync, tests record it.
        assert_eq!(renamed.lock().unwrap().len(), 2);
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

        let update = tag_videos(&ids, &["beach".to_owned()], &engine, |_, _| {});

        assert_eq!(update.tagged.len(), 1);
        assert_eq!(update.failed, 1);
        assert!(update.problem.is_some());
    }
}
