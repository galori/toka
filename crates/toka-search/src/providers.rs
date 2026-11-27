use crate::search::{tokenize_query, SearchError, SearchProvider};
use crate::search_log::SearchLogContext;
use std::path::PathBuf;

pub struct OwnedIndexSearchProvider {
    database: PathBuf,
    read_only: bool,
}

impl OwnedIndexSearchProvider {
    pub fn new(database: PathBuf) -> Self {
        Self {
            database,
            read_only: false,
        }
    }

    /// A provider for `ts` searches that must leave the index unchanged:
    /// every open is read-only, so a version 1 database is never
    /// migrated and stays usable by a version 1 app. The app keeps the
    /// migrating [`OwnedIndexSearchProvider::new`]. (SQLite may still
    /// touch `-wal`/`-shm` sidecars beside the database while reading.)
    pub fn new_read_only(database: PathBuf) -> Self {
        Self {
            database,
            read_only: true,
        }
    }

    fn open_index(&self) -> Result<crate::owned_index::OwnedIndex, SearchError> {
        let index = if self.read_only {
            crate::owned_index::OwnedIndex::open_read_only(&self.database)
        } else {
            crate::owned_index::OwnedIndex::open(&self.database)
        };
        index.map_err(|error| SearchError::Provider(error.to_string()))
    }
}

/// Keeps the paths a search may show: under a watched root, with no
/// hidden ancestor (a dotfile, or a macOS-hidden folder, anywhere from
/// the root down). The visibility half of [`SearchProvider`], factored
/// out so the app's `ts`-backed provider — which learns its roots from
/// `ts folders list` instead of the database — filters exactly as the
/// owned-index provider does.
pub fn retain_visible_under_roots(roots: &[PathBuf], paths: &mut Vec<PathBuf>) {
    paths.retain(|path| {
        roots.iter().any(|root| {
            path.starts_with(root)
                && crate::index_visibility::hidden_ancestor(root, path)
                    .is_ok_and(|hidden| hidden.is_none())
        })
    });
}

impl SearchProvider for OwnedIndexSearchProvider {
    fn candidates(
        &self,
        query: &str,
        _whole_path: bool,
        _context: &SearchLogContext,
    ) -> Result<Vec<PathBuf>, SearchError> {
        let terms = query_terms(query)?;
        self.open_index()?
            .search_candidates(&terms)
            .map_err(|error| SearchError::Provider(error.to_string()))
    }

    fn retain_visible_paths(&self, paths: &mut Vec<PathBuf>) -> Result<(), SearchError> {
        let roots = self
            .open_index()?
            .roots()
            .map(|roots| roots.into_iter().map(|(_, path)| path).collect::<Vec<_>>())
            .map_err(|error| SearchError::Provider(error.to_string()))?;
        retain_visible_under_roots(&roots, paths);
        Ok(())
    }

    fn revision(&self) -> u64 {
        self.open_index()
            .and_then(|index| {
                index
                    .revision()
                    .map_err(|error| SearchError::Provider(error.to_string()))
            })
            .unwrap_or(0)
    }
}

fn query_terms(query: &str) -> Result<Vec<String>, SearchError> {
    let tokens = tokenize_query(query)?;
    let mut terms = Vec::new();
    let mut needs_all_candidates = false;
    for token in tokens {
        if token == "(" || token == ")" || token.eq_ignore_ascii_case("AND") {
            continue;
        }
        if token.eq_ignore_ascii_case("NOT") {
            needs_all_candidates = true;
            continue;
        }
        if token.eq_ignore_ascii_case("OR") {
            continue;
        }
        if let Some((field, value)) = token.split_once(':') {
            match field.to_ascii_lowercase().as_str() {
                "size" | "created" | "modified" | "type" | "extension" | "ext" => {
                    needs_all_candidates = true;
                    continue;
                }
                "tags" | "tag" | "filename" | "file" | "name" | "path" | "folder"
                    if !value.is_empty() =>
                {
                    terms.push(value.to_owned());
                    continue;
                }
                _ => {}
            }
        }
        terms.push(token);
    }
    if needs_all_candidates {
        // A metadata or negative term can match a file regardless of its name.
        // Ask each provider for its complete indexed set, then let SearchEngine
        // apply the full expression and media filter locally.
        terms = vec!["*".into()];
    }
    if terms.is_empty() {
        Err(SearchError::InvalidQuery)
    } else {
        Ok(terms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn owned_index_provider_excludes_hidden_and_out_of_scope_paths() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir_all(root.join(".worktrees")).unwrap();
        let root = root.canonicalize().unwrap();
        let database = directory.path().join("state").join("index.sqlite3");
        crate::owned_index::OwnedIndexManager::under(database.clone())
            .add_folder(&root)
            .unwrap();
        let provider = OwnedIndexSearchProvider::new(database);
        let visible = root.join("clip.mp4");
        let mut paths = vec![
            visible.clone(),
            root.join(".clip.mp4"),
            root.join(".worktrees/clip.mp4"),
            directory.path().join("outside.mp4"),
        ];

        provider.retain_visible_paths(&mut paths).unwrap();

        assert_eq!(paths, vec![visible]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn owned_index_search_excludes_a_folder_hidden_after_indexing() {
        use crate::search::{
            MediaType, SearchEngine, SearchFields, SearchRequest, SearchSort, PAGE_SIZE,
        };

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("media");
        let archive = root.join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let visible = root.join("clip-visible.mp4");
        let hidden = archive.join("clip-hidden.mp4");
        std::fs::write(&visible, b"video").unwrap();
        std::fs::write(&hidden, b"video").unwrap();
        let root = root.canonicalize().unwrap();
        let database = directory.path().join("state").join("index.sqlite3");
        let state = crate::owned_index::OwnedIndexManager::under(database.clone())
            .add_folder(&root)
            .unwrap();
        crate::owned_index::OwnedIndex::open(&database)
            .unwrap()
            .scan_root(&state.folders[0].id, &root, |_| {})
            .unwrap();
        assert!(std::process::Command::new("/usr/bin/chflags")
            .arg("hidden")
            .arg(&archive)
            .status()
            .unwrap()
            .success());

        let engine = SearchEngine::new(Arc::new(OwnedIndexSearchProvider::new(database)));
        let result = engine
            .search(SearchRequest {
                query: "clip".into(),
                page: 1,
                page_size: PAGE_SIZE,
                search_id: None,
                fields: SearchFields::default(),
                media_type: MediaType::Videos,
                sort: SearchSort::default(),
            })
            .unwrap();

        assert_eq!(result.total_results, 1);
        assert_eq!(result.results[0].file_name, "clip-visible.mp4");
        assert!(std::process::Command::new("/usr/bin/chflags")
            .arg("nohidden")
            .arg(archive)
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn visible_helper_keeps_only_paths_under_visible_roots() {
        let root = PathBuf::from("/media");
        let visible = root.join("clip.mp4");
        let mut paths = vec![
            visible.clone(),
            root.join(".clip.mp4"),
            root.join(".worktrees/clip.mp4"),
            PathBuf::from("/elsewhere/clip.mp4"),
        ];

        retain_visible_under_roots(&[root], &mut paths);

        assert_eq!(paths, vec![visible]);
    }

    #[test]
    fn providers_keep_quoted_values_together() {
        assert_eq!(
            query_terms("filename:\"family trip\" OR folder:archive").unwrap(),
            ["family trip", "archive"]
        );
    }

    #[test]
    fn metadata_and_negative_terms_request_every_indexed_path() {
        for query in [
            "type:image",
            "extension:jpg",
            "NOT tags:hidden",
            "filename:trip OR NOT folder:private",
        ] {
            assert_eq!(query_terms(query).unwrap(), ["*"], "query: {query}");
        }
    }
}
