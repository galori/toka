mod providers;
mod search;

#[cfg(target_os = "macos")]
use providers::MdfindSearchProvider;
#[cfg(target_os = "linux")]
use providers::RecollSearchProvider;
use search::{SearchEngine, SearchError, SearchPage, SearchProvider, SearchRequest};
use serde::Serialize;
use std::sync::Arc;
use tauri::{Manager, State};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreparedVideo {
    file_path: String,
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
    })
}

fn platform_provider() -> Arc<dyn SearchProvider> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(MdfindSearchProvider::system())
    }
    #[cfg(target_os = "linux")]
    {
        Arc::new(RecollSearchProvider::system())
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Arc::new(SearchEngine::new(platform_provider())))
        .invoke_handler(tauri::generate_handler![search_videos, prepare_video])
        .run(tauri::generate_context!())
        .expect("error while running Toka");
}
