mod providers;
mod search;

#[cfg(all(not(feature = "e2e"), target_os = "macos"))]
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
    #[cfg(all(not(feature = "e2e"), target_os = "macos"))]
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
    #[cfg(all(
        not(feature = "e2e"),
        not(any(target_os = "macos", target_os = "linux"))
    ))]
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

    builder
        .manage(Arc::new(SearchEngine::new(platform_provider())))
        .invoke_handler(tauri::generate_handler![search_videos, prepare_video])
        .run(tauri::generate_context!())
        .expect("error while running Toka");
}
