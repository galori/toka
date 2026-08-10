use crate::tags;
use crate::thumbnails;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub const PAGE_SIZE: usize = 24;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub query: String,
    pub page: usize,
    pub page_size: usize,
    #[serde(default)]
    pub fields: SearchFields,
}

/// Which part of a video a query is matched against. The three are separate
/// haystacks rather than nested ones, so switching one off cannot be undone by
/// another: the tag block belongs to the tags, the file name is read without
/// it, and the path is the folders the file sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchFields {
    pub tags: bool,
    pub file_name: bool,
    pub path: bool,
}

impl SearchFields {
    fn any(&self) -> bool {
        self.tags || self.file_name || self.path
    }
}

/// What a search covered before the fields could be chosen: the whole file
/// name, tag block included, and nothing of the folders above it.
impl Default for SearchFields {
    fn default() -> Self {
        Self {
            tags: true,
            file_name: true,
            path: false,
        }
    }
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
    #[error("Choose at least one of tags, file name or path to search.")]
    NoSearchFields,
    #[error("The requested search page is invalid.")]
    InvalidPage,
    #[error("{0}")]
    Provider(String),
    #[error("That video is no longer available.")]
    VideoUnavailable,
}

pub trait SearchProvider: Send + Sync {
    /// Candidate videos for `query`, which the engine then filters by the
    /// fields the viewer chose. `whole_path` asks for the wider set a folder
    /// search needs: every index Toka reads answers on file names alone unless
    /// it is told otherwise, and a video in a matching folder has no reason to
    /// carry the term in its own name.
    fn candidates(&self, query: &str, whole_path: bool) -> Result<Vec<PathBuf>, SearchError>;
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
    /// The order the search on screen is being read in. Only the current one is
    /// kept: a new question replaces it, and there is never a second search to
    /// go back to — the frontend asks for page one and then walks forward.
    shuffle: Mutex<Option<Shuffle>>,
}

/// The seed a search's order was drawn from, under the question that asked for
/// it. Pages after the first reuse the seed so they continue that order instead
/// of drawing a new one, which would repeat some videos and skip others.
struct Shuffle {
    query: String,
    fields: SearchFields,
    seed: u64,
}

impl SearchEngine {
    pub fn new(provider: Arc<dyn SearchProvider>) -> Self {
        Self {
            provider,
            result_paths: Mutex::new(HashMap::new()),
            renamed_paths: Mutex::new(HashMap::new()),
            shuffle: Mutex::new(None),
        }
    }

    pub fn search(&self, request: SearchRequest) -> Result<SearchPage, SearchError> {
        let query = request.query.trim();
        if query.is_empty() {
            return Err(SearchError::InvalidQuery);
        }
        let expr = parse_query(query)?;
        if !request.fields.any() {
            return Err(SearchError::NoSearchFields);
        }
        if request.page == 0 || request.page_size != PAGE_SIZE {
            return Err(SearchError::InvalidPage);
        }

        let fields = request.fields;
        let needs_whole_path = fields.path || query_needs_whole_path(&expr);
        let mut seen = HashSet::new();
        let mut paths = self
            .candidates(query, needs_whole_path)?
            .into_iter()
            .filter(|path| is_supported_video(path))
            .filter(|path| matches_query(path, &expr, fields))
            .filter(|path| seen.insert(path.clone()))
            .collect::<Vec<_>>();

        // Sorted not to be read in this order — the shuffle below undoes it —
        // but so that the shuffle has something fixed to work from. A provider
        // is free to answer the same query in a different order each time, and
        // page two has to continue the order page one started.
        paths.sort_by(|left, right| {
            let left_name = left.file_name().unwrap_or_default().to_string_lossy();
            let right_name = right.file_name().unwrap_or_default().to_string_lossy();
            left_name
                .to_lowercase()
                .cmp(&right_name.to_lowercase())
                .then_with(|| left.cmp(right))
        });
        shuffle(&mut paths, self.shuffle_seed(query, fields, request.page));

        let total_results = paths.len();
        let total_pages = total_results.div_ceil(PAGE_SIZE);
        let start = (request.page - 1).saturating_mul(PAGE_SIZE);
        let page_paths = paths.into_iter().skip(start).take(PAGE_SIZE);
        let mut known_paths = self.result_paths.lock().unwrap();
        let results = page_paths
            .map(|path| {
                let result = video_result(&path);
                known_paths.insert(result.id.clone(), path);
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

    /// The seed the order of this page is drawn from. Page one is a new
    /// question and always gets a new one, so asking the same thing twice shows
    /// a different part of what matched rather than the same screenful again.
    /// The pages after it continue whatever order page one started.
    fn shuffle_seed(&self, query: &str, fields: SearchFields, page: usize) -> u64 {
        let mut current = self.shuffle.lock().unwrap();
        if page > 1 {
            if let Some(shuffle) = current.as_ref() {
                if shuffle.query == query && shuffle.fields == fields {
                    return shuffle.seed;
                }
            }
        }
        let seed = fresh_seed();
        *current = Some(Shuffle {
            query: query.to_owned(),
            fields,
            seed,
        });
        seed
    }

    /// The provider's candidates, corrected for the renames this session has
    /// made: a stale name is replaced by the name its file carries now, and
    /// every renamed file the index has already dropped is added back. The
    /// caller still filters these by the search terms, so a rename only shows
    /// up when the query matches the name the file actually has.
    fn candidates(&self, query: &str, whole_path: bool) -> Result<Vec<PathBuf>, SearchError> {
        let mut paths = self.provider.candidates(query, whole_path)?;
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

    /// A page of results for videos Toka was handed rather than found, so a
    /// playlist file plays through everything a search's results do — the
    /// player, its drawer, tagging, thumbnails — without a second path through
    /// any of it. `paths` keeps the order it was given, because that is the
    /// order a playlist asked for.
    ///
    /// The page is whole: it holds every video there is, so nothing about it
    /// invites the frontend to ask for a second page that no search backs.
    pub fn page_of_videos(&self, query: String, paths: Vec<PathBuf>) -> SearchPage {
        let mut known_paths = self.result_paths.lock().unwrap();
        let results: Vec<VideoResult> = paths
            .into_iter()
            .map(|path| {
                let result = video_result(&path);
                known_paths.insert(result.id.clone(), path);
                result
            })
            .collect();
        SearchPage {
            query,
            page: 1,
            page_size: PAGE_SIZE,
            total_results: results.len(),
            total_pages: 1,
            results,
        }
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

    /// The frames a preview runs through for the video behind `result_id`.
    pub fn preview_paths(&self, result_id: &str) -> Result<Vec<PathBuf>, SearchError> {
        let path = self.video_path(result_id)?;
        thumbnails::preview(&path).ok_or(SearchError::VideoUnavailable)
    }
}

/// A seed nothing can predict, and never the same one twice: the clock alone
/// repeats itself when two searches land inside the same tick, which the
/// counter separates.
fn fresh_seed() -> u64 {
    static SEARCHES: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or_default();
    let searches = SEARCHES.fetch_add(1, Ordering::Relaxed);
    let mut state = now ^ searches.wrapping_mul(GOLDEN_GAMMA);
    next_random(&mut state)
}

/// Toka carries no random number generator, and does not need one it could not
/// reproduce: SplitMix64 is a handful of arithmetic, and a seed is enough to
/// draw the same order again for the pages that follow.
const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

fn next_random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(GOLDEN_GAMMA);
    let mut drawn = *state;
    drawn = (drawn ^ (drawn >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    drawn = (drawn ^ (drawn >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    drawn ^ (drawn >> 31)
}

/// Fisher-Yates, so every order is as likely as every other one.
fn shuffle(paths: &mut [PathBuf], seed: u64) {
    let mut state = seed;
    for index in (1..paths.len()).rev() {
        let swap = (next_random(&mut state) % (index as u64 + 1)) as usize;
        paths.swap(index, swap);
    }
}

/// A result for `path` under a fresh id, which is what the frontend names the
/// video by from then on: an id the engine minted, never a filesystem path.
fn video_result(path: &Path) -> VideoResult {
    VideoResult {
        id: Uuid::new_v4().to_string(),
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
        tags: tags::get(path),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    Tags,
    FileName,
    Path,
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Expr {
    Term { field: Field, value: String },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

fn tokenize(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in query.chars() {
        if ch == '(' || ch == ')' {
            if !current.is_empty() {
                tokens.push(current.clone());
                current.clear();
            }
            tokens.push(ch.to_string());
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(current.clone());
                current.clear();
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn parse_term_token(token: &str) -> Expr {
    if let Some(colon) = token.find(':') {
        let prefix = token[..colon].to_ascii_lowercase();
        let suffix = &token[colon + 1..];
        if !suffix.is_empty() {
            let field = match prefix.as_str() {
                "tags" | "tag" => Some(Field::Tags),
                "filename" | "file" | "name" => Some(Field::FileName),
                "path" => Some(Field::Path),
                _ => None,
            };
            if let Some(field) = field {
                return Expr::Term {
                    field,
                    value: suffix.to_lowercase(),
                };
            }
        }
    }
    Expr::Term {
        field: Field::Any,
        value: token.to_lowercase(),
    }
}

fn parse_query(query: &str) -> Result<Expr, SearchError> {
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return Err(SearchError::InvalidQuery);
    }
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.parse_expr()?;
    if parser.pos != parser.tokens.len() {
        return Err(SearchError::InvalidQuery);
    }
    Ok(expr)
}

struct Parser {
    tokens: Vec<String>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.pos).map(|s| s.as_str())
    }

    fn consume(&mut self) -> Option<String> {
        if self.pos < self.tokens.len() {
            let tok = self.tokens[self.pos].clone();
            self.pos += 1;
            Some(tok)
        } else {
            None
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, SearchError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, SearchError> {
        let mut left = self.parse_and()?;
        while let Some(tok) = self.peek() {
            if tok.eq_ignore_ascii_case("OR") {
                self.consume();
                // OR must be followed by an operand
                if self.peek().is_none() {
                    return Err(SearchError::InvalidQuery);
                }
                if self.peek().is_some_and(|t| {
                    t.eq_ignore_ascii_case("OR") || t.eq_ignore_ascii_case("AND") || t == ")"
                }) {
                    return Err(SearchError::InvalidQuery);
                }
                let right = self.parse_and()?;
                left = Expr::Or(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, SearchError> {
        let mut left = self.parse_primary()?;
        while let Some(peek) = self.peek() {
            if peek.eq_ignore_ascii_case("OR") || peek == ")" {
                break;
            }
            if peek.eq_ignore_ascii_case("AND") {
                self.consume();
                if self.peek().is_none() {
                    return Err(SearchError::InvalidQuery);
                }
                if self.peek().is_some_and(|t| {
                    t.eq_ignore_ascii_case("OR") || t.eq_ignore_ascii_case("AND") || t == ")"
                }) {
                    return Err(SearchError::InvalidQuery);
                }
                // explicit AND consumed, fall through to parse next primary
            } else if peek == "("
                || !peek.eq_ignore_ascii_case("AND") && !peek.eq_ignore_ascii_case("OR")
            {
                // implicit AND: no token consumed yet
            } else {
                break;
            }
            // guard double operator
            if let Some(ntok) = self.peek() {
                if ntok.eq_ignore_ascii_case("AND") || ntok.eq_ignore_ascii_case("OR") {
                    return Err(SearchError::InvalidQuery);
                }
            }
            let right = self.parse_primary()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_primary(&mut self) -> Result<Expr, SearchError> {
        let tok = self.peek().ok_or(SearchError::InvalidQuery)?.to_string();
        if tok == "(" {
            self.consume();
            if self.peek().is_none() {
                return Err(SearchError::InvalidQuery);
            }
            if self.peek() == Some(")") {
                return Err(SearchError::InvalidQuery);
            }
            let expr = self.parse_expr()?;
            if self.peek() != Some(")") {
                return Err(SearchError::InvalidQuery);
            }
            self.consume();
            Ok(expr)
        } else if tok == ")" || tok.eq_ignore_ascii_case("AND") || tok.eq_ignore_ascii_case("OR") {
            Err(SearchError::InvalidQuery)
        } else {
            self.consume();
            Ok(parse_term_token(&tok))
        }
    }
}

fn query_needs_whole_path(expr: &Expr) -> bool {
    match expr {
        Expr::Term { field, .. } => *field == Field::Path,
        Expr::And(l, r) | Expr::Or(l, r) => query_needs_whole_path(l) || query_needs_whole_path(r),
    }
}

fn matches_query(path: &Path, expr: &Expr, fields: SearchFields) -> bool {
    match expr {
        Expr::Term { field, value } => match field {
            Field::Any => {
                let haystacks = haystacks(path, fields);
                haystacks.iter().any(|hay| hay.contains(value))
            }
            Field::Tags => tags_haystack(path).contains(value),
            Field::FileName => file_name_haystack(path).contains(value),
            Field::Path => path_haystack(path).contains(value),
        },
        Expr::And(left, right) => {
            matches_query(path, left, fields) && matches_query(path, right, fields)
        }
        Expr::Or(left, right) => {
            matches_query(path, left, fields) || matches_query(path, right, fields)
        }
    }
}

fn tags_haystack(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    tags::Tags::parse_file_name(&name).into_values().join(" ")
}

fn file_name_haystack(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    tags::Tags::default().apply_to_file(&name).to_lowercase()
}

fn path_haystack(path: &Path) -> String {
    path.parent()
        .unwrap_or_else(|| Path::new(""))
        .to_string_lossy()
        .to_lowercase()
}

/// Whether every term is somewhere in the parts of `path` the viewer chose to
/// search. A term may land in any one of them, so a query can name a folder and
/// a tag at once; all of them still have to land somewhere, which is what makes
/// a second word narrow a search rather than widen it.
#[allow(dead_code)]
fn matches_terms(path: &Path, terms: &[String], fields: SearchFields) -> bool {
    let haystacks = haystacks(path, fields);
    terms
        .iter()
        .all(|term| haystacks.iter().any(|hay| hay.contains(term)))
}

/// The lowercased text of each part of `path` that is being searched. The tags
/// are read from the name rather than the filesystem, so this stays free of the
/// per-candidate `stat` a lookup would cost.
fn haystacks(path: &Path, fields: SearchFields) -> Vec<String> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let mut haystacks = Vec::with_capacity(3);
    if fields.tags {
        haystacks.push(tags::Tags::parse_file_name(&name).into_values().join(" "));
    }
    if fields.file_name {
        haystacks.push(tags::Tags::default().apply_to_file(&name).to_lowercase());
    }
    if fields.path {
        haystacks.push(
            path.parent()
                .unwrap_or_else(|| Path::new(""))
                .to_string_lossy()
                .to_lowercase(),
        );
    }
    haystacks
}

pub fn is_supported_video(path: &Path) -> bool {
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
        whole_path_asks: Mutex<Vec<bool>>,
    }

    impl FakeProvider {
        fn new(paths: Vec<PathBuf>) -> Self {
            Self {
                paths: Mutex::new(paths),
                whole_path_asks: Mutex::new(Vec::new()),
            }
        }
    }

    impl SearchProvider for FakeProvider {
        fn candidates(&self, _query: &str, whole_path: bool) -> Result<Vec<PathBuf>, SearchError> {
            self.whole_path_asks.lock().unwrap().push(whole_path);
            Ok(self.paths.lock().unwrap().clone())
        }
    }

    fn request(query: &str) -> SearchRequest {
        SearchRequest {
            query: query.into(),
            page: 1,
            page_size: PAGE_SIZE,
            fields: SearchFields::default(),
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

        let provider = Arc::new(FakeProvider::new(vec![
            wrong_type,
            missing_term,
            matching.clone(),
        ]));
        let page = SearchEngine::new(provider)
            .search(request("family SUMMER"))
            .unwrap();

        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "Summer Family Vacation.MP4");
        assert_eq!(page.results[0].extension, "mp4");
        assert!(!page.results[0].id.is_empty());
    }

    #[test]
    fn search_lists_provider_video_paths_without_filesystem_access() {
        let protected = PathBuf::from("/protected/Downloads/Untitled.mov");
        let provider = Arc::new(FakeProvider::new(vec![protected.clone()]));

        let page = SearchEngine::new(provider)
            .search(request("untitled"))
            .unwrap();

        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "Untitled.mov");
        assert_eq!(page.results[0].extension, "mov");
    }

    /// `count` clips named `clip-00.mp4` upwards, handed to the provider in
    /// reverse so nothing downstream can pass by keeping the order it was given.
    fn clips(directory: &Path, count: usize) -> Vec<PathBuf> {
        (0..count)
            .rev()
            .map(|number| {
                let path = directory.join(format!("clip-{number:02}.mp4"));
                fs::write(&path, b"test").unwrap();
                path
            })
            .collect()
    }

    fn names(page: &SearchPage) -> Vec<String> {
        page.results
            .iter()
            .map(|result| result.file_name.clone())
            .collect()
    }

    /// Every name `clips` wrote, in the order `sort` puts them in, so a set of
    /// results can be compared for what it holds rather than for its order.
    fn every_clip_name(count: usize) -> Vec<String> {
        (0..count)
            .map(|number| format!("clip-{number:02}.mp4"))
            .collect()
    }

    fn page_two(query: &str) -> SearchRequest {
        SearchRequest {
            page: 2,
            ..request(query)
        }
    }

    #[test]
    fn search_deduplicates_and_paginates_results() {
        let directory = tempdir().unwrap();
        let mut paths = clips(directory.path(), 25);
        paths.push(paths[0].clone());

        let engine = SearchEngine::new(Arc::new(FakeProvider::new(paths)));
        let first = engine.search(request("clip")).unwrap();
        let second = engine.search(page_two("clip")).unwrap();

        assert_eq!(first.total_results, 25);
        assert_eq!(first.total_pages, 2);
        assert_eq!(first.results.len(), 24);
        assert_eq!(second.results.len(), 1);

        let mut every_name = [names(&first), names(&second)].concat();
        every_name.sort();
        assert_eq!(every_name, every_clip_name(25));
    }

    /// An alphabetical list buries everything past the first screenful, which
    /// is the wrong answer to "find me something to watch".
    #[test]
    fn search_shuffles_results_rather_than_listing_them_by_name() {
        let directory = tempdir().unwrap();
        let paths = clips(directory.path(), 30);

        let engine = SearchEngine::new(Arc::new(FakeProvider::new(paths)));
        let found = names(&engine.search(request("clip")).unwrap());

        let mut by_name = found.clone();
        by_name.sort();
        assert_eq!(found.len(), 24);
        assert_ne!(found, by_name);
    }

    /// The pages after the first continue the order the first one started. A
    /// fresh shuffle per page would repeat some videos and skip others.
    #[test]
    fn search_keeps_one_order_across_the_pages_of_a_search() {
        let directory = tempdir().unwrap();
        let paths = clips(directory.path(), 30);

        let engine = SearchEngine::new(Arc::new(FakeProvider::new(paths)));
        let first = names(&engine.search(request("clip")).unwrap());
        let second = names(&engine.search(page_two("clip")).unwrap());

        let mut every_name = [first, second].concat();
        every_name.sort();
        assert_eq!(every_name, every_clip_name(30));
    }

    /// Asking the same question again is a new search, and gets a new order —
    /// otherwise a viewer who did not like what came back has no way to see the
    /// rest of what matched.
    #[test]
    fn search_gives_the_same_query_a_new_order_each_time() {
        let directory = tempdir().unwrap();
        let paths = clips(directory.path(), 30);

        let engine = SearchEngine::new(Arc::new(FakeProvider::new(paths)));
        let first = names(&engine.search(request("clip")).unwrap());
        let second = names(&engine.search(request("clip")).unwrap());

        assert_ne!(first, second);
    }

    #[test]
    fn search_covers_the_whole_file_name_including_its_tags_by_default() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("Beach day [home summer].mp4");
        fs::write(&video, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![video]));
        let engine = SearchEngine::new(provider.clone());

        assert_eq!(
            engine.search(request("beach home")).unwrap().total_results,
            1
        );
        // The folder is not part of that default, so the provider is never
        // asked for the wider candidate set either.
        assert_eq!(*provider.whole_path_asks.lock().unwrap(), [false]);
    }

    #[test]
    fn searching_tags_alone_ignores_the_rest_of_the_name() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("Beach day [home].mp4");
        fs::write(&video, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![video])));
        let tags_only = |query: &str| SearchRequest {
            fields: SearchFields {
                tags: true,
                file_name: false,
                path: false,
            },
            ..request(query)
        };

        assert_eq!(engine.search(tags_only("home")).unwrap().total_results, 1);
        assert_eq!(engine.search(tags_only("beach")).unwrap().total_results, 0);
    }

    #[test]
    fn searching_the_file_name_alone_ignores_its_tag_block() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("Beach day [home].mp4");
        fs::write(&video, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![video])));
        let name_only = |query: &str| SearchRequest {
            fields: SearchFields {
                tags: false,
                file_name: true,
                path: false,
            },
            ..request(query)
        };

        assert_eq!(engine.search(name_only("beach")).unwrap().total_results, 1);
        assert_eq!(engine.search(name_only("home")).unwrap().total_results, 0);
    }

    #[test]
    fn searching_the_path_matches_the_folder_and_widens_the_candidate_set() {
        let directory = tempdir().unwrap();
        let folder = directory.path().join("Holiday");
        fs::create_dir(&folder).unwrap();
        let video = folder.join("clip.mp4");
        fs::write(&video, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![video]));
        let engine = SearchEngine::new(provider.clone());
        let with_path = |path: bool| SearchRequest {
            fields: SearchFields {
                tags: true,
                file_name: true,
                path,
            },
            ..request("holiday")
        };

        assert_eq!(engine.search(with_path(false)).unwrap().total_results, 0);
        assert_eq!(engine.search(with_path(true)).unwrap().total_results, 1);
        assert_eq!(*provider.whole_path_asks.lock().unwrap(), [false, true]);
    }

    #[test]
    fn every_term_may_match_a_different_selected_field() {
        let directory = tempdir().unwrap();
        let folder = directory.path().join("Holiday");
        fs::create_dir(&folder).unwrap();
        let video = folder.join("clip [home].mp4");
        fs::write(&video, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![video])));
        let all_fields = |query: &str| SearchRequest {
            fields: SearchFields {
                tags: true,
                file_name: true,
                path: true,
            },
            ..request(query)
        };

        assert_eq!(
            engine
                .search(all_fields("holiday clip home"))
                .unwrap()
                .total_results,
            1
        );
        assert_eq!(
            engine
                .search(all_fields("holiday absent"))
                .unwrap()
                .total_results,
            0
        );
    }

    #[test]
    fn a_search_with_nothing_selected_is_refused() {
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(Vec::new())));

        let error = engine
            .search(SearchRequest {
                fields: SearchFields {
                    tags: false,
                    file_name: false,
                    path: false,
                },
                ..request("clip")
            })
            .unwrap_err();

        assert!(matches!(error, SearchError::NoSearchFields), "{error}");
    }

    #[test]
    fn a_renamed_video_is_found_under_its_new_name_while_the_index_is_stale() {
        let directory = tempdir().unwrap();
        let indexed = directory.path().join("sample1.mp4");
        fs::write(&indexed, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![indexed.clone()]));
        let engine = SearchEngine::new(provider.clone());
        let first = engine.search(request("sample")).unwrap();

        let tagged = directory.path().join("sample1 [home].mp4");
        fs::rename(&indexed, &tagged).unwrap();
        engine
            .update_video_path(&first.results[0].id, tagged.clone())
            .unwrap();

        // The index still lists the name the file had before it was tagged.
        let stale = engine.search(request("sample")).unwrap();
        assert_eq!(stale.total_results, 1);
        assert_eq!(stale.results[0].file_name, "sample1 [home].mp4");
        assert_eq!(stale.results[0].tags, ["home"]);

        // The index has since dropped the name, because the file behind it is
        // gone; the renamed file must not disappear with it.
        provider.paths.lock().unwrap().clear();
        let dropped = engine.search(request("sample")).unwrap();
        assert_eq!(dropped.total_results, 1);
        assert_eq!(dropped.results[0].file_name, "sample1 [home].mp4");

        // Once the index catches up the file is listed once, not twice.
        *provider.paths.lock().unwrap() = vec![tagged];
        let refreshed = engine.search(request("sample")).unwrap();
        assert_eq!(refreshed.total_results, 1);
    }

    #[test]
    fn a_renamed_video_is_only_found_by_terms_matching_its_new_name() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("sample1.mp4");
        fs::write(&video, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![video.clone()]));
        let engine = SearchEngine::new(provider.clone());
        let first = engine.search(request("sample")).unwrap();
        let tagged = directory.path().join("sample1 [home].mp4");
        fs::rename(&video, &tagged).unwrap();
        engine
            .update_video_path(&first.results[0].id, tagged)
            .unwrap();
        provider.paths.lock().unwrap().clear();

        let page = engine.search(request("holiday")).unwrap();

        assert_eq!(page.total_results, 0);
    }

    #[test]
    fn a_video_renamed_back_to_its_indexed_name_is_listed_once() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("sample1.mp4");
        fs::write(&video, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![video.clone()]));
        let engine = SearchEngine::new(provider);
        let request = || request("sample");
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
        let provider = Arc::new(FakeProvider::new(vec![video.clone()]));
        let engine = SearchEngine::new(provider.clone());
        let first = engine.search(request("sample")).unwrap();
        let tagged = directory.path().join("sample1 [home].mp4");
        fs::rename(&video, &tagged).unwrap();
        engine
            .update_video_path(&first.results[0].id, tagged.clone())
            .unwrap();
        provider.paths.lock().unwrap().clear();
        fs::remove_file(&tagged).unwrap();

        let page = engine.search(request("sample")).unwrap();

        assert_eq!(page.total_results, 0);
    }

    #[test]
    fn videos_handed_to_the_engine_become_a_playable_page_in_the_order_given() {
        let directory = tempdir().unwrap();
        let beach = directory.path().join("Beach day [home].mp4");
        let party = directory.path().join("party.MKV");
        for path in [&beach, &party] {
            fs::write(path, b"test").unwrap();
        }
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(Vec::new())));

        let page = engine.page_of_videos("summer.m3u8".into(), vec![party.clone(), beach.clone()]);

        assert_eq!(page.query, "summer.m3u8");
        assert_eq!(page.total_results, 2);
        assert_eq!(page.total_pages, 1);
        // A playlist's order is the order it asked to be played in, so these
        // are not sorted by name the way a search's results are.
        assert_eq!(page.results[0].file_name, "party.MKV");
        assert_eq!(page.results[0].extension, "mkv");
        assert_eq!(page.results[1].file_name, "Beach day [home].mp4");
        assert_eq!(page.results[1].tags, ["home"]);
        // And each result plays the video it stands for.
        assert_eq!(engine.video_path(&page.results[0].id).unwrap(), party);
        assert_eq!(engine.video_path(&page.results[1].id).unwrap(), beach);
    }

    #[test]
    fn a_result_id_remains_valid_after_a_later_search() {
        let directory = tempdir().unwrap();
        let first_path = directory.path().join("first-clip.mp4");
        let second_path = directory.path().join("second-clip.mp4");
        fs::write(&first_path, b"test").unwrap();
        fs::write(&second_path, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![first_path.clone()]));
        let engine = SearchEngine::new(provider.clone());

        let first_page = engine.search(request("first")).unwrap();
        *provider.paths.lock().unwrap() = vec![second_path];
        engine.search(request("second")).unwrap();

        assert_eq!(
            engine.video_path(&first_page.results[0].id).unwrap(),
            first_path
        );
    }

    #[test]
    fn or_matches_either_term() {
        let directory = tempdir().unwrap();
        let beach = directory.path().join("beach.mp4");
        let mountain = directory.path().join("mountain.mp4");
        let other = directory.path().join("city.mp4");
        for path in [&beach, &mountain, &other] {
            fs::write(path, b"test").unwrap();
        }
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![
            beach.clone(),
            mountain.clone(),
            other.clone(),
        ])));

        let page = engine.search(request("beach OR mountain")).unwrap();
        assert_eq!(page.total_results, 2);
        let mut names = names(&page);
        names.sort();
        assert_eq!(names, vec!["beach.mp4", "mountain.mp4"]);

        // case-insensitive OR
        let page2 = engine.search(request("beach or mountain")).unwrap();
        assert_eq!(page2.total_results, 2);
    }

    #[test]
    fn and_explicit_requires_both_terms() {
        let directory = tempdir().unwrap();
        let both = directory.path().join("beach mountain.mp4");
        let one = directory.path().join("beach.mp4");
        for path in [&both, &one] {
            fs::write(path, b"test").unwrap();
        }
        let engine =
            SearchEngine::new(Arc::new(FakeProvider::new(vec![both.clone(), one.clone()])));

        assert_eq!(
            engine
                .search(request("beach AND mountain"))
                .unwrap()
                .total_results,
            1
        );
        assert_eq!(
            engine
                .search(request("beach AND mountain"))
                .unwrap()
                .results[0]
                .file_name,
            "beach mountain.mp4"
        );
        // implicit AND keeps backward compatibility
        assert_eq!(
            engine
                .search(request("beach mountain"))
                .unwrap()
                .total_results,
            1
        );
    }

    #[test]
    fn parentheses_group_or_before_and() {
        let directory = tempdir().unwrap();
        let beach_party = directory.path().join("beach party.mp4");
        let mountain_party = directory.path().join("mountain party.mp4");
        let beach_only = directory.path().join("beach.mp4");
        for path in [&beach_party, &mountain_party, &beach_only] {
            fs::write(path, b"test").unwrap();
        }
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![
            beach_party.clone(),
            mountain_party.clone(),
            beach_only.clone(),
        ])));

        // (beach OR mountain) AND party => both with party
        let page = engine
            .search(request("(beach OR mountain) AND party"))
            .unwrap();
        assert_eq!(page.total_results, 2);

        // beach OR (mountain AND party) => beach_only + mountain_party + beach_party
        let page2 = engine
            .search(request("beach OR mountain AND party"))
            .unwrap();
        // AND binds tighter than OR, so mountain AND party requires both; beach matches any beach file
        assert_eq!(page2.total_results, 3);

        // Explicit parentheses same as implicit precedence
        let page3 = engine
            .search(request("beach OR (mountain AND party)"))
            .unwrap();
        assert_eq!(page3.total_results, 3);
    }

    #[test]
    fn and_binds_tighter_than_or_without_parentheses() {
        let directory = tempdir().unwrap();
        let a = directory.path().join("a.mp4");
        let b = directory.path().join("b.mp4");
        let ab = directory.path().join("a b.mp4");
        for path in [&a, &b, &ab] {
            fs::write(path, b"test").unwrap();
        }
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![
            a.clone(),
            b.clone(),
            ab.clone(),
        ])));
        // a OR b AND a => a OR (b AND a) => a, ab
        let page = engine.search(request("a OR b AND a")).unwrap();
        assert_eq!(page.total_results, 2);
    }

    #[test]
    fn field_prefix_tags_restricts_to_tags() {
        let directory = tempdir().unwrap();
        let tagged = directory.path().join("Beach day [home].mp4");
        let untagged = directory.path().join("home video.mp4");
        for path in [&tagged, &untagged] {
            fs::write(path, b"test").unwrap();
        }
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![
            tagged.clone(),
            untagged.clone(),
        ])));

        // tags:home matches only the tagged file's tag block
        let page = engine.search(request("tags:home")).unwrap();
        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "Beach day [home].mp4");

        // tags:beach should find nothing because beach is in filename, not tags
        assert_eq!(
            engine.search(request("tags:beach")).unwrap().total_results,
            0
        );

        // case-insensitive prefix
        assert_eq!(
            engine.search(request("TAGS:HOME")).unwrap().total_results,
            1
        );
        assert_eq!(engine.search(request("tag:home")).unwrap().total_results, 1);
    }

    #[test]
    fn field_prefix_filename_restricts_to_file_name_without_tags() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("Beach day [home].mp4");
        fs::write(&video, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![video])));

        // filename:beach matches the name part (without tag block)
        assert_eq!(
            engine
                .search(request("filename:beach"))
                .unwrap()
                .total_results,
            1
        );
        // filename:home should not match because home is only in tags
        assert_eq!(
            engine
                .search(request("filename:home"))
                .unwrap()
                .total_results,
            0
        );
        // also file: alias
        assert_eq!(
            engine.search(request("file:beach")).unwrap().total_results,
            1
        );
    }

    #[test]
    fn field_prefix_path_restricts_to_folders_and_widens_candidates() {
        let directory = tempdir().unwrap();
        let folder = directory.path().join("Holiday");
        fs::create_dir(&folder).unwrap();
        let video = folder.join("clip.mp4");
        fs::write(&video, b"test").unwrap();
        let other_folder = directory.path().join("Work");
        fs::create_dir(&other_folder).unwrap();
        let other_video = other_folder.join("clip.mp4");
        fs::write(&other_video, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![video.clone(), other_video.clone()]));
        let engine = SearchEngine::new(provider.clone());

        // path:holiday should match only video in Holiday folder
        let page = engine.search(request("path:holiday")).unwrap();
        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "clip.mp4");
        // provider should have been asked with whole_path true because query uses path: prefix
        assert_eq!(
            *provider.whole_path_asks.lock().unwrap().last().unwrap(),
            true
        );

        // filename:clip with path restriction should not match path term
        assert_eq!(
            engine.search(request("path:work")).unwrap().total_results,
            1
        );
        // path:absent finds nothing
        assert_eq!(
            engine.search(request("path:absent")).unwrap().total_results,
            0
        );
    }

    #[test]
    fn field_prefix_combined_with_boolean_operators() {
        let directory = tempdir().unwrap();
        let beach_home = directory.path().join("beach [home].mp4");
        let mountain_home = directory.path().join("mountain [home].mp4");
        let beach_work = directory.path().join("beach [work].mp4");
        for path in [&beach_home, &mountain_home, &beach_work] {
            fs::write(path, b"test").unwrap();
        }
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![
            beach_home.clone(),
            mountain_home.clone(),
            beach_work.clone(),
        ])));

        // tags:home AND filename:beach => only beach_home
        let page = engine
            .search(request("tags:home AND filename:beach"))
            .unwrap();
        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "beach [home].mp4");

        // tags:home OR tags:work with filename:beach => beach_home and beach_work
        let page2 = engine
            .search(request("(tags:home OR tags:work) AND filename:beach"))
            .unwrap();
        assert_eq!(page2.total_results, 2);

        // tags:home OR filename:mountain => beach_home, mountain_home
        let page3 = engine
            .search(request("tags:home OR filename:mountain"))
            .unwrap();
        assert_eq!(page3.total_results, 2);
    }

    #[test]
    fn plain_terms_remain_and_across_all_fields() {
        let directory = tempdir().unwrap();
        let folder = directory.path().join("Holiday");
        fs::create_dir(&folder).unwrap();
        let video = folder.join("beach [home].mp4");
        fs::write(&video, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![video])));

        let all_fields = SearchRequest {
            fields: SearchFields {
                tags: true,
                file_name: true,
                path: true,
            },
            ..request("holiday beach home")
        };
        assert_eq!(engine.search(all_fields).unwrap().total_results, 1);
    }

    #[test]
    fn invalid_queries_are_rejected() {
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(Vec::new())));
        assert!(matches!(
            engine.search(request("")).unwrap_err(),
            SearchError::InvalidQuery
        ));
        assert!(matches!(
            engine.search(request("   ")).unwrap_err(),
            SearchError::InvalidQuery
        ));
        assert!(matches!(
            engine.search(request("AND beach")).unwrap_err(),
            SearchError::InvalidQuery
        ));
        assert!(matches!(
            engine.search(request("beach OR")).unwrap_err(),
            SearchError::InvalidQuery
        ));
        assert!(matches!(
            engine.search(request("(beach")).unwrap_err(),
            SearchError::InvalidQuery
        ));
        assert!(matches!(
            engine.search(request("beach)")).unwrap_err(),
            SearchError::InvalidQuery
        ));
        assert!(matches!(
            engine.search(request("()")).unwrap_err(),
            SearchError::InvalidQuery
        ));
        assert!(matches!(
            engine.search(request("beach OR OR mountain")).unwrap_err(),
            SearchError::InvalidQuery
        ));
    }

    #[test]
    fn parentheses_without_spaces_are_tokenized() {
        let directory = tempdir().unwrap();
        let beach = directory.path().join("beach.mp4");
        let mountain = directory.path().join("mountain.mp4");
        for path in [&beach, &mountain] {
            fs::write(path, b"test").unwrap();
        }
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![beach, mountain])));
        // no space after '(' or before ')'
        let page = engine.search(request("(beach OR mountain)")).unwrap();
        assert_eq!(page.total_results, 2);
    }
}
