use crate::search::{MediaType, SearchFields};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use url::Url;

const BUILT_IN_ADDONS: [(&str, &str); 1] = [(
    "built-in/tiktok.json",
    include_str!("../addons/tiktok.json"),
)];
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddonActionSummary {
    pub addon_id: String,
    pub action_id: String,
    pub label: String,
    pub effects: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AddonMenu {
    pub actions: Vec<AddonActionSummary>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddonSearchLaunch {
    pub query: String,
    pub fields: SearchFields,
    pub media_type: MediaType,
    pub addon_id: String,
    pub capture: String,
    pub value: String,
}

pub trait ActionHost {
    fn open_related_search(&mut self, search: AddonSearchLaunch) -> Result<(), String>;
    fn open_url(&mut self, url: &Url) -> Result<(), String>;
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AddonManifest {
    schema_version: u32,
    id: String,
    name: String,
    #[serde(rename = "match")]
    match_rule: MatchRule,
    actions: Vec<AddonAction>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MatchRule {
    file_name_regex: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AddonAction {
    id: String,
    label: String,
    steps: Vec<ActionStep>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
enum SearchField {
    Tags,
    FileName,
    Path,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum ActionStep {
    OpenRelatedSearch {
        capture: String,
        query: String,
        fields: Vec<SearchField>,
        media_type: MediaType,
    },
    OpenUrl {
        url: String,
    },
}

struct CompiledAddon {
    manifest: AddonManifest,
    file_name_regex: Regex,
}

pub struct AddonRegistry {
    addons: Vec<CompiledAddon>,
    errors: Vec<String>,
}

impl AddonRegistry {
    pub fn system(user_directory: Option<&std::path::Path>) -> Self {
        Self::from_json_sources_and_directory(BUILT_IN_ADDONS, user_directory)
    }

    pub fn from_json_sources<'a>(sources: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut registry = Self {
            addons: Vec::new(),
            errors: Vec::new(),
        };
        for (source, json) in sources {
            registry.push_json(source, json);
        }
        registry
    }

    pub fn from_json_sources_and_directory<'a>(
        sources: impl IntoIterator<Item = (&'a str, &'a str)>,
        directory: Option<&std::path::Path>,
    ) -> Self {
        let mut registry = Self::from_json_sources(sources);
        let Some(directory) = directory else {
            return registry;
        };
        let mut entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries.filter_map(Result::ok).collect::<Vec<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return registry,
            Err(error) => {
                registry.errors.push(format!(
                    "{}: add-ons could not be read: {error}",
                    directory.display()
                ));
                return registry;
            }
        };
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            let path = entry.path();
            if !path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
                || !entry.file_type().is_ok_and(|kind| kind.is_file())
            {
                continue;
            }
            let source = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            match std::fs::read_to_string(&path) {
                Ok(json) => registry.push_json(&source, &json),
                Err(error) => registry
                    .errors
                    .push(format!("{source}: add-on could not be read: {error}")),
            }
        }
        registry
    }

    fn push_json(&mut self, source: &str, json: &str) {
        match compile_manifest(json) {
            Ok(addon)
                if !self
                    .addons
                    .iter()
                    .any(|loaded| loaded.manifest.id == addon.manifest.id) =>
            {
                self.addons.push(addon);
            }
            Ok(addon) => self.errors.push(format!(
                "{source}: add-on id '{}' is already loaded.",
                addon.manifest.id
            )),
            Err(message) => self.errors.push(format!("{source}: {message}")),
        }
    }

    pub fn validate_related_search(&self, search: &AddonSearchLaunch) -> Result<(), String> {
        let addon = self
            .addons
            .iter()
            .find(|addon| addon.manifest.id == search.addon_id)
            .ok_or_else(|| "The add-on used for this search is not loaded.".to_owned())?;
        if addon
            .file_name_regex
            .capture_names()
            .flatten()
            .any(|capture| capture == search.capture)
        {
            Ok(())
        } else {
            Err(format!(
                "The add-on no longer defines capture '{}'.",
                search.capture
            ))
        }
    }

    pub fn matches_related_search(
        &self,
        search: &AddonSearchLaunch,
        file_name: &str,
    ) -> Result<bool, String> {
        let addon = self
            .addons
            .iter()
            .find(|addon| addon.manifest.id == search.addon_id)
            .ok_or_else(|| "The add-on used for this search is not loaded.".to_owned())?;
        if !addon
            .file_name_regex
            .capture_names()
            .flatten()
            .any(|capture| capture == search.capture)
        {
            return Err(format!(
                "The add-on no longer defines capture '{}'.",
                search.capture
            ));
        }
        Ok(addon
            .file_name_regex
            .captures(file_name)
            .and_then(|captures| captures.name(&search.capture))
            .is_some_and(|capture| capture.as_str() == search.value))
    }

    pub fn run_action(
        &self,
        file_name: &str,
        addon_id: &str,
        action_id: &str,
        host: &mut impl ActionHost,
    ) -> Result<(), String> {
        let addon = self
            .addons
            .iter()
            .find(|addon| addon.manifest.id == addon_id)
            .ok_or_else(|| "That add-on is not loaded.".to_owned())?;
        let captures = addon
            .file_name_regex
            .captures(file_name)
            .ok_or_else(|| "That add-on does not match this file.".to_owned())?;
        let action = addon
            .manifest
            .actions
            .iter()
            .find(|action| action.id == action_id)
            .ok_or_else(|| "That add-on action is not loaded.".to_owned())?;
        for step in &action.steps {
            match step {
                ActionStep::OpenRelatedSearch {
                    capture,
                    query,
                    fields,
                    media_type,
                } => {
                    let value = capture_value(&captures, capture)?;
                    host.open_related_search(AddonSearchLaunch {
                        query: render_template(query, &captures, str::to_owned)?,
                        fields: search_fields(fields),
                        media_type: *media_type,
                        addon_id: addon.manifest.id.clone(),
                        capture: capture.clone(),
                        value: value.to_owned(),
                    })?;
                }
                ActionStep::OpenUrl { url } => {
                    let rendered = render_template(url, &captures, encode_url_capture)?;
                    let parsed = Url::parse(&rendered)
                        .map_err(|error| format!("The add-on URL could not be built: {error}"))?;
                    host.open_url(&parsed)?;
                }
            }
        }
        Ok(())
    }

    pub fn matching_actions(&self, file_name: &str) -> AddonMenu {
        let actions = self
            .addons
            .iter()
            .filter(|addon| addon.file_name_regex.captures(file_name).is_some())
            .flat_map(|addon| {
                addon
                    .manifest
                    .actions
                    .iter()
                    .map(|action| AddonActionSummary {
                        addon_id: addon.manifest.id.clone(),
                        action_id: action.id.clone(),
                        label: action.label.clone(),
                        effects: action.steps.iter().map(ActionStep::effect).collect(),
                    })
            })
            .collect();
        AddonMenu {
            actions,
            errors: self.errors.clone(),
        }
    }
}

impl ActionStep {
    fn effect(&self) -> String {
        match self {
            Self::OpenRelatedSearch { .. } => "New Toka window".into(),
            Self::OpenUrl { url } => Url::parse(url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| "Website".into()),
        }
    }
}

fn capture_value<'a>(captures: &'a regex::Captures<'_>, name: &str) -> Result<&'a str, String> {
    captures
        .name(name)
        .map(|capture| capture.as_str())
        .ok_or_else(|| format!("This file has no value for capture '{name}'."))
}

fn render_template(
    template: &str,
    captures: &regex::Captures<'_>,
    encode: fn(&str) -> String,
) -> Result<String, String> {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        rendered.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        let close = after_open
            .find('}')
            .ok_or_else(|| "The add-on template is invalid.".to_owned())?;
        let name = &after_open[..close];
        rendered.push_str(&encode(capture_value(captures, name)?));
        rest = &after_open[close + 1..];
    }
    rendered.push_str(rest);
    Ok(rendered)
}

fn encode_url_capture(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'@') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn search_fields(fields: &[SearchField]) -> SearchFields {
    let mut selected = SearchFields {
        tags: false,
        file_name: false,
        path: false,
    };
    for field in fields {
        match field {
            SearchField::Tags => selected.tags = true,
            SearchField::FileName => selected.file_name = true,
            SearchField::Path => selected.path = true,
        }
    }
    selected
}

fn compile_manifest(json: &str) -> Result<CompiledAddon, String> {
    let manifest: AddonManifest =
        serde_json::from_str(json).map_err(|error| format!("invalid JSON: {error}"))?;
    if manifest.schema_version != 1 {
        return Err(format!(
            "schema version {} is not supported.",
            manifest.schema_version
        ));
    }
    if !valid_id(&manifest.id) || manifest.name.trim().is_empty() {
        return Err("add-on id must be a non-empty name, and its name must not be empty.".into());
    }
    if manifest.actions.is_empty() {
        return Err("an add-on must define at least one action.".into());
    }
    let file_name_regex = Regex::new(&manifest.match_rule.file_name_regex)
        .map_err(|error| format!("invalid fileNameRegex: {error}"))?;
    let captures = file_name_regex
        .capture_names()
        .flatten()
        .collect::<HashSet<_>>();
    let mut action_ids = HashSet::new();
    for action in &manifest.actions {
        if !valid_id(&action.id) || action.label.trim().is_empty() {
            return Err("action ids and labels must not be empty.".into());
        }
        if !action_ids.insert(action.id.as_str()) {
            return Err(format!(
                "action id '{}' is defined more than once.",
                action.id
            ));
        }
        if action.steps.is_empty() {
            return Err(format!(
                "action '{}' must define at least one step.",
                action.id
            ));
        }
        for step in &action.steps {
            match step {
                ActionStep::OpenRelatedSearch {
                    capture,
                    query,
                    fields,
                    ..
                } => {
                    require_capture(capture, &captures)?;
                    validate_template(query, &captures)?;
                    if fields.is_empty() {
                        return Err("openRelatedSearch must name at least one field.".into());
                    }
                }
                ActionStep::OpenUrl { url } => validate_url_template(url, &captures)?,
            }
        }
    }
    Ok(CompiledAddon {
        manifest,
        file_name_regex,
    })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn require_capture(capture: &str, captures: &HashSet<&str>) -> Result<(), String> {
    if captures.contains(capture) {
        Ok(())
    } else {
        Err(format!(
            "capture '{capture}' is not defined by fileNameRegex."
        ))
    }
}

fn validate_template(template: &str, captures: &HashSet<&str>) -> Result<(), String> {
    for capture in template_captures(template)? {
        require_capture(capture, captures)?;
    }
    Ok(())
}

fn template_captures(template: &str) -> Result<Vec<&str>, String> {
    let mut captures = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        if rest[..open].contains('}') {
            return Err("action template has an unmatched '}'.".into());
        }
        let after_open = &rest[open + 1..];
        let close = after_open
            .find('}')
            .ok_or_else(|| "action template has an unmatched '{'.".to_owned())?;
        let capture = &after_open[..close];
        if !valid_id(capture) {
            return Err("action template contains an invalid capture name.".into());
        }
        captures.push(capture);
        rest = &after_open[close + 1..];
    }
    if rest.contains('}') {
        return Err("action template has an unmatched '}'.".into());
    }
    Ok(captures)
}

fn validate_url_template(template: &str, captures: &HashSet<&str>) -> Result<(), String> {
    if !template.starts_with("https://") {
        return Err("openUrl requires an HTTPS URL.".into());
    }
    let authority = template["https://".len()..]
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('{') || authority.contains('}') {
        return Err("openUrl placeholders are not allowed in the URL host.".into());
    }
    validate_template(template, captures)?;
    let mut example = template.to_owned();
    for capture in template_captures(template)? {
        example = example.replace(&format!("{{{capture}}}"), "capture");
    }
    let url = Url::parse(&example).map_err(|error| format!("openUrl is invalid: {error}"))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err("openUrl requires an HTTPS URL with a fixed host.".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("openUrl must not contain login details.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIKTOK: &str = include_str!("../addons/tiktok.json");

    #[test]
    fn related_search_filter_excludes_similar_creator_handles() {
        let registry = AddonRegistry::from_json_sources([("tiktok.json", TIKTOK)]);
        let search = AddonSearchLaunch {
            query: "@sam".into(),
            fields: SearchFields {
                tags: false,
                file_name: true,
                path: false,
            },
            media_type: MediaType::Videos,
            addon_id: "tiktok".into(),
            capture: "handle".into(),
            value: "@sam".into(),
        };

        assert!(registry
            .matches_related_search(&search, "2026-10-10_@sam_18938383490812348343.mp4")
            .unwrap());
        assert!(!registry
            .matches_related_search(&search, "2026-10-10_@sammy_18938383490812348344.mp4")
            .unwrap());
    }

    #[derive(Debug, PartialEq)]
    enum HostedAction {
        Search(AddonSearchLaunch),
        Url(String),
    }

    #[derive(Default)]
    struct FakeHost(Vec<HostedAction>);

    impl ActionHost for FakeHost {
        fn open_related_search(&mut self, search: AddonSearchLaunch) -> Result<(), String> {
            self.0.push(HostedAction::Search(search));
            Ok(())
        }

        fn open_url(&mut self, url: &Url) -> Result<(), String> {
            self.0.push(HostedAction::Url(url.as_str().to_owned()));
            Ok(())
        }
    }

    #[test]
    fn creator_action_runs_the_new_window_before_the_browser() {
        let registry = AddonRegistry::from_json_sources([("tiktok.json", TIKTOK)]);
        let mut host = FakeHost::default();

        registry
            .run_action(
                "2026-10-10_@sometiktoker_18938383490812348343.mp4",
                "tiktok",
                "creator",
                &mut host,
            )
            .unwrap();

        assert_eq!(
            host.0,
            [
                HostedAction::Search(AddonSearchLaunch {
                    query: "@sometiktoker".into(),
                    fields: SearchFields {
                        tags: false,
                        file_name: true,
                        path: false,
                    },
                    media_type: MediaType::Videos,
                    addon_id: "tiktok".into(),
                    capture: "handle".into(),
                    value: "@sometiktoker".into(),
                }),
                HostedAction::Url("https://tiktok.com/@sometiktoker".into()),
            ]
        );
    }

    #[test]
    fn unsafe_or_unbound_action_steps_are_rejected() {
        let http = TIKTOK.replace("https://tiktok.com/{handle}", "http://tiktok.com/{handle}");
        let dynamic_host = TIKTOK.replace(
            "https://tiktok.com/{handle}",
            "https://{handle}.example.com/creator",
        );
        let missing_capture = TIKTOK.replace("\"capture\": \"handle\"", "\"capture\": \"missing\"");

        let registry = AddonRegistry::from_json_sources([
            ("http.json", http.as_str()),
            ("dynamic-host.json", dynamic_host.as_str()),
            ("missing-capture.json", missing_capture.as_str()),
        ]);
        let menu = registry.matching_actions("2026-10-10_@sometiktoker_18938383490812348343.mp4");

        assert!(menu.actions.is_empty(), "{:?}", menu.actions);
        assert_eq!(menu.errors.len(), 3, "{:?}", menu.errors);
        assert!(menu.errors.iter().any(|error| error.contains("HTTPS")));
        assert!(menu.errors.iter().any(|error| error.contains("host")));
        assert!(menu.errors.iter().any(|error| error.contains("missing")));
    }

    #[test]
    fn malformed_user_addons_are_reported_without_blocking_valid_addons() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("broken.json"), "{not json").unwrap();
        std::fs::write(directory.path().join("notes.txt"), "ignored").unwrap();

        let registry = AddonRegistry::from_json_sources_and_directory(
            [("tiktok.json", TIKTOK)],
            Some(directory.path()),
        );
        let menu = registry.matching_actions("2026-10-10_@sometiktoker_18938383490812348343.mp4");

        assert_eq!(menu.actions.len(), 1);
        assert_eq!(menu.errors.len(), 1);
        assert!(menu.errors[0].contains("broken.json"), "{:?}", menu.errors);
        assert!(menu.errors[0].contains("invalid JSON"), "{:?}", menu.errors);
    }

    #[test]
    fn sample_tiktok_filename_exposes_the_creator_action() {
        let registry = AddonRegistry::from_json_sources([("tiktok.json", TIKTOK)]);

        let menu = registry.matching_actions("2026-10-10_@sometiktoker_18938383490812348343.mp4");

        assert_eq!(menu.errors, Vec::<String>::new());
        assert_eq!(
            menu.actions,
            [AddonActionSummary {
                addon_id: "tiktok".into(),
                action_id: "creator".into(),
                label: "Open TikTok creator".into(),
                effects: vec!["New Toka window".into(), "tiktok.com".into()],
            }]
        );
    }
}
