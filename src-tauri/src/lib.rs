#[cfg(target_os = "linux")]
mod player_linux;
mod providers;
mod search;

#[cfg(target_os = "macos")]
use providers::MdfindSearchProvider;
#[cfg(target_os = "linux")]
use providers::{PlocateSearchProvider, RecollSearchProvider};
use search::{SearchEngine, SearchError, SearchPage, SearchProvider, SearchRequest};
use serde::Serialize;
use std::sync::Arc;
use tauri::{Manager, State};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreparedVideo {
    file_path: String,
    playback_backend: &'static str,
}

#[derive(Serialize)]
struct CommandError {
    kind: &'static str,
    message: String,
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
fn set_native_speed(speed: f64, player: State<'_, Arc<player_linux::NativePlayer>>) -> Result<(), CommandError> {
    player_linux::set_speed(player.inner(), speed).map_err(playback_error)
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

struct InvalidProvider(String);
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

    let builder = builder.manage(Arc::new(SearchEngine::new(platform_provider())));
    #[cfg(target_os = "linux")]
    let builder = if cfg!(all(feature = "e2e", not(feature = "native-e2e"))) {
        builder.invoke_handler(tauri::generate_handler![search_videos, prepare_video])
    } else {
        let player = player_linux::NativePlayer::new();
        let setup_player = player.clone();
        builder
            .manage(player)
            .setup(move |app| player_linux::install(app, setup_player.clone()))
            .invoke_handler(tauri::generate_handler![
                search_videos,
                prepare_video,
                load_native_video,
                set_native_paused,
                set_native_speed,
                native_video_rotation,
                set_native_video_rotation,
                seek_native_video,
                native_playback_state,
                stop_native_video,
                set_native_video_bounds
            ])
    };
    #[cfg(not(target_os = "linux"))]
    let builder = builder.invoke_handler(tauri::generate_handler![search_videos, prepare_video]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running Toka");
}
