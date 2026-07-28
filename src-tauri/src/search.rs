use crate::tags;
use crate::thumbnails;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use uuid::Uuid;

pub const PAGE_SIZE: usize = 24;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub query: String,
    pub page: usize,
    pub page_size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoResult {
    pub id: String,
    pub file_name: String,
    pub extension: String,
    pub thumbnail_path: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchPage {
    pub query: String,
    pub page: usize,
    pub page_size: usize,
    pub total_results: usize,
    pub total_pages: usize,
    pub results: Vec<VideoResult>,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("Enter at least one search term.")]
    InvalidQuery,
    #[error("The requested search page is invalid.")]
    InvalidPage,
    #[error("{0}")]
    Provider(String),
    #[error("That video is no longer available.")]
    VideoUnavailable,
}

pub trait SearchProvider: Send + Sync {
    fn candidates(&self, query: &str) -> Result<Vec<PathBuf>, SearchError>;
}

pub struct SearchEngine {
    provider: Arc<dyn SearchProvider>,
    result_paths: Mutex<HashMap<String, PathBuf>>,
    /// Files this session has renamed, indexed by the name the search provider
    /// knew them under. Every provider Toka uses answers from an index that is
    /// rebuilt on a schedule, so a file tagged a moment ago is still indexed
    /// under its old name — and an index that filters out names whose file is
    /// gone, as `plocate --existing` does, stops listing it at all. Keeping the
    /// renames here lets a search see the file the way it is now.
    renamed_paths: Mutex<HashMap<PathBuf, PathBuf>>,
}

impl SearchEngine {
    pub fn new(provider: Arc<dyn SearchProvider>) -> Self {
        Self {
            provider,
            result_paths: Mutex::new(HashMap::new()),
            renamed_paths: Mutex::new(HashMap::new()),
        }
    }

    pub fn search(&self, request: SearchRequest) -> Result<SearchPage, SearchError> {
        let query = request.query.trim();
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|term| term.to_lowercase())
            .collect();
        if terms.is_empty() {
            return Err(SearchError::InvalidQuery);
        }
        if request.page == 0 || request.page_size != PAGE_SIZE {
            return Err(SearchError::InvalidPage);
        }

        let mut seen = HashSet::new();
        let mut paths = self
            .candidates(query)?
            .into_iter()
            .filter(|path| is_supported_video(path))
            .filter(|path| {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase();
                terms.iter().all(|term| name.contains(term))
            })
            .filter(|path| seen.insert(path.clone()))
            .collect::<Vec<_>>();

        paths.sort_by(|left, right| {
            let left_name = left.file_name().unwrap_or_default().to_string_lossy();
            let right_name = right.file_name().unwrap_or_default().to_string_lossy();
            left_name
                .to_lowercase()
                .cmp(&right_name.to_lowercase())
                .then_with(|| left.cmp(right))
        });

        let total_results = paths.len();
        let total_pages = total_results.div_ceil(PAGE_SIZE);
        let start = (request.page - 1).saturating_mul(PAGE_SIZE);
        let page_paths = paths.into_iter().skip(start).take(PAGE_SIZE);
        let mut known_paths = self.result_paths.lock().unwrap();
        let results = page_paths
            .map(|path| {
                let id = Uuid::new_v4().to_string();
                let result = VideoResult {
                    id: id.clone(),
                    file_name: path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    extension: path
                        .extension()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase(),
                    thumbnail_path: None,
                    tags: tags::get(&path),
                };
                known_paths.insert(id, path);
                result
            })
            .collect();

        Ok(SearchPage {
            query: query.to_owned(),
            page: request.page,
            page_size: PAGE_SIZE,
            total_results,
            total_pages,
            results,
        })
    }

    /// The provider's candidates, corrected for the renames this session has
    /// made: a stale name is replaced by the name its file carries now, and
    /// every renamed file the index has already dropped is added back. The
    /// caller still filters these by the search terms, so a rename only shows
    /// up when the query matches the name the file actually has.
    fn candidates(&self, query: &str) -> Result<Vec<PathBuf>, SearchError> {
        let mut paths = self.provider.candidates(query)?;
        let renamed = self.renamed_paths.lock().unwrap();
        if renamed.is_empty() {
            return Ok(paths);
        }
        for path in &mut paths {
            if let Some(current) = renamed.get(path) {
                *path = current.clone();
            }
        }
        // A renamed file that has since been deleted is not a search result,
        // and this is the only place a path Toka renamed itself can be checked
        // cheaply — provider candidates are deliberately left untouched.
        paths.extend(renamed.values().filter(|path| path.is_file()).cloned());
        Ok(paths)
    }

    pub fn video_path(&self, result_id: &str) -> Result<PathBuf, SearchError> {
        let path = self
            .result_paths
            .lock()
            .unwrap()
            .get(result_id)
            .cloned()
            .ok_or(SearchError::VideoUnavailable)?;
        if path.is_file() && is_supported_video(&path) {
            Ok(path)
        } else {
            Err(SearchError::VideoUnavailable)
        }
    }

    pub fn update_video_path(&self, result_id: &str, path: PathBuf) -> Result<(), SearchError> {
        let mut known_paths = self.result_paths.lock().unwrap();
        let known_path = known_paths
            .get_mut(result_id)
            .ok_or(SearchError::VideoUnavailable)?;
        let previous = std::mem::replace(known_path, path.clone());
        if previous != path {
            self.record_rename(previous, path);
        }
        Ok(())
    }

    /// Records a rename under the name the index still knows the file by.
    /// Renaming the same file again replaces that one entry rather than
    /// chaining, and renaming it back to its indexed name drops the entry, so
    /// the index's own answer is enough again.
    fn record_rename(&self, previous: PathBuf, current: PathBuf) {
        let mut renamed = self.renamed_paths.lock().unwrap();
        let indexed = renamed
            .iter()
            .find(|(_, path)| **path == previous)
            .map(|(indexed, _)| indexed.clone())
            .unwrap_or(previous);
        if indexed == current {
            renamed.remove(&indexed);
        } else {
            renamed.insert(indexed, current);
        }
    }

    pub fn thumbnail_path(&self, result_id: &str) -> Result<PathBuf, SearchError> {
        let path = self.video_path(result_id)?;
        thumbnails::generate(&path).ok_or(SearchError::VideoUnavailable)
    }
}

fn is_supported_video(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_lowercase)
            .as_deref(),
        Some("mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" | "mpeg" | "mpg" | "mpe")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::Mutex};
    use tempfile::tempdir;

    struct FakeProvider {
        paths: Mutex<Vec<PathBuf>>,
    }

    impl SearchProvider for FakeProvider {
        fn candidates(&self, _query: &str) -> Result<Vec<PathBuf>, SearchError> {
            Ok(self.paths.lock().unwrap().clone())
        }
    }

    #[test]
    fn search_returns_only_supported_files_containing_every_term() {
        let directory = tempdir().unwrap();
        let matching = directory.path().join("Summer Family Vacation.MP4");
        let missing_term = directory.path().join("summer-party.mov");
        let wrong_type = directory.path().join("summer-family-vacation.txt");
        for path in [&matching, &missing_term, &wrong_type] {
            fs::write(path, b"test").unwrap();
        }

        let provider = Arc::new(FakeProvider {
            paths: Mutex::new(vec![wrong_type, missing_term, matching.clone()]),
        });
        let page = SearchEngine::new(provider)
            .search(SearchRequest {
                query: "family SUMMER".into(),
                page: 1,
                page_size: PAGE_SIZE,
            })
            .unwrap();

        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "Summer Family Vacation.MP4");
        assert_eq!(page.results[0].extension, "mp4");
        assert!(!page.results[0].id.is_empty());
    }

    #[test]
    fn search_lists_provider_video_paths_without_filesystem_access() {
        let protected = PathBuf::from("/protected/Downloads/Untitled.mov");
        let provider = Arc::new(FakeProvider {
            paths: Mutex::new(vec![protected.clone()]),
        });

        let page = SearchEngine::new(provider)
            .search(SearchRequest {
                query: "untitled".into(),
                page: 1,
                page_size: PAGE_SIZE,
            })
            .unwrap();

        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "Untitled.mov");
        assert_eq!(page.results[0].extension, "mov");
    }

    #[test]
    fn search_sorts_deduplicates_and_paginates_results() {
        let directory = tempdir().unwrap();
        let mut paths = Vec::new();
        for number in (0..25).rev() {
            let path = directory.path().join(format!("clip-{number:02}.mp4"));
            fs::write(&path, b"test").unwrap();
            paths.push(path);
        }
        paths.push(paths[0].clone());

        let provider = Arc::new(FakeProvider {
            paths: Mutex::new(paths),
        });
        let page = SearchEngine::new(provider)
            .search(SearchRequest {
                query: "clip".into(),
                page: 2,
                page_size: PAGE_SIZE,
            })
            .unwrap();

        assert_eq!(page.total_results, 25);
        assert_eq!(page.total_pages, 2);
        assert_eq!(page.results.len(), 1);
        assert_eq!(page.results[0].file_name, "clip-24.mp4");
    }

    #[test]
    fn a_renamed_video_is_found_under_its_new_name_while_the_index_is_stale() {
        let directory = tempdir().unwrap();
        let indexed = directory.path().join("sample1.mp4");
        fs::write(&indexed, b"test").unwrap();
        let provider = Arc::new(FakeProvider {
            paths: Mutex::new(vec![indexed.clone()]),
        });
        let engine = SearchEngine::new(provider.clone());
        let first = engine
            .search(SearchRequest {
                query: "sample".into(),
                page: 1,
                page_size: PAGE_SIZE,
            })
            .unwrap();

        let tagged = directory.path().join("sample1 [home].mp4");
        fs::rename(&indexed, &tagged).unwrap();
        engine
            .update_video_path(&first.results[0].id, tagged.clone())
            .unwrap();

        // The index still lists the name the file had before it was tagged.
        let stale = engine
            .search(SearchRequest {
                query: "sample".into(),
                page: 1,
                page_size: PAGE_SIZE,
            })
            .unwrap();
        assert_eq!(stale.total_results, 1);
        assert_eq!(stale.results[0].file_name, "sample1 [home].mp4");
        assert_eq!(stale.results[0].tags, ["home"]);

        // The index has since dropped the name, because the file behind it is
        // gone; the renamed file must not disappear with it.
        provider.paths.lock().unwrap().clear();
        let dropped = engine
            .search(SearchRequest {
                query: "sample".into(),
                page: 1,
                page_size: PAGE_SIZE,
            })
            .unwrap();
        assert_eq!(dropped.total_results, 1);
        assert_eq!(dropped.results[0].file_name, "sample1 [home].mp4");

        // Once the index catches up the file is listed once, not twice.
        *provider.paths.lock().unwrap() = vec![tagged];
        let refreshed = engine
            .search(SearchRequest {
                query: "sample".into(),
                page: 1,
                page_size: PAGE_SIZE,
            })
            .unwrap();
        assert_eq!(refreshed.total_results, 1);
    }

    #[test]
    fn a_renamed_video_is_only_found_by_terms_matching_its_new_name() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("sample1.mp4");
        fs::write(&video, b"test").unwrap();
        let provider = Arc::new(FakeProvider {
            paths: Mutex::new(vec![video.clone()]),
        });
        let engine = SearchEngine::new(provider.clone());
        let first = engine
            .search(SearchRequest {
                query: "sample".into(),
                page: 1,
                page_size: PAGE_SIZE,
            })
            .unwrap();
        let tagged = directory.path().join("sample1 [home].mp4");
        fs::rename(&video, &tagged).unwrap();
        engine
            .update_video_path(&first.results[0].id, tagged)
            .unwrap();
        provider.paths.lock().unwrap().clear();

        let page = engine
            .search(SearchRequest {
                query: "holiday".into(),
                page: 1,
                page_size: PAGE_SIZE,
            })
            .unwrap();

        assert_eq!(page.total_results, 0);
    }

    #[test]
    fn a_video_renamed_back_to_its_indexed_name_is_listed_once() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("sample1.mp4");
        fs::write(&video, b"test").unwrap();
        let provider = Arc::new(FakeProvider {
            paths: Mutex::new(vec![video.clone()]),
        });
        let engine = SearchEngine::new(provider);
        let request = || SearchRequest {
            query: "sample".into(),
            page: 1,
            page_size: PAGE_SIZE,
        };
        let first = engine.search(request()).unwrap();

        let tagged = directory.path().join("sample1 [home].mp4");
        fs::rename(&video, &tagged).unwrap();
        engine
            .update_video_path(&first.results[0].id, tagged.clone())
            .unwrap();
        let tagged_page = engine.search(request()).unwrap();
        fs::rename(&tagged, &video).unwrap();
        engine
            .update_video_path(&tagged_page.results[0].id, video)
            .unwrap();

        let page = engine.search(request()).unwrap();

        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "sample1.mp4");
    }

    #[test]
    fn a_renamed_video_that_is_gone_is_not_listed() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("sample1.mp4");
        fs::write(&video, b"test").unwrap();
        let provider = Arc::new(FakeProvider {
            paths: Mutex::new(vec![video.clone()]),
        });
        let engine = SearchEngine::new(provider.clone());
        let first = engine
            .search(SearchRequest {
                query: "sample".into(),
                page: 1,
                page_size: PAGE_SIZE,
            })
            .unwrap();
        let tagged = directory.path().join("sample1 [home].mp4");
        fs::rename(&video, &tagged).unwrap();
        engine
            .update_video_path(&first.results[0].id, tagged.clone())
            .unwrap();
        provider.paths.lock().unwrap().clear();
        fs::remove_file(&tagged).unwrap();

        let page = engine
            .search(SearchRequest {
                query: "sample".into(),
                page: 1,
                page_size: PAGE_SIZE,
            })
            .unwrap();

        assert_eq!(page.total_results, 0);
    }

    #[test]
    fn a_result_id_remains_valid_after_a_later_search() {
        let directory = tempdir().unwrap();
        let first_path = directory.path().join("first-clip.mp4");
        let second_path = directory.path().join("second-clip.mp4");
        fs::write(&first_path, b"test").unwrap();
        fs::write(&second_path, b"test").unwrap();
        let provider = Arc::new(FakeProvider {
            paths: Mutex::new(vec![first_path.clone()]),
        });
        let engine = SearchEngine::new(provider.clone());

        let first_page = engine
            .search(SearchRequest {
                query: "first".into(),
                page: 1,
                page_size: PAGE_SIZE,
            })
            .unwrap();
        *provider.paths.lock().unwrap() = vec![second_path];
        engine
            .search(SearchRequest {
                query: "second".into(),
                page: 1,
                page_size: PAGE_SIZE,
            })
            .unwrap();

        assert_eq!(
            engine.video_path(&first_page.results[0].id).unwrap(),
            first_path
        );
    }
}
