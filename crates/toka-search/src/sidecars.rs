//! Subtitle files sitting beside a video, shared by tag renames (which move
//! them along) and the app (which offers them as tracks).
//!
//! Web conversion (`to_web_vtt`) stays in the app; only discovery lives here.

use std::path::{Path, PathBuf};

/// Sidecar formats Toka recognises next to a video. The web media engine can
/// only render the text formats, so `web_playable` reports which of these
/// survive conversion to WebVTT.
pub const SUBTITLE_EXTENSIONS: [&str; 5] = ["srt", "vtt", "ass", "ssa", "sub"];

#[derive(Debug, PartialEq, Eq)]
pub struct SidecarSubtitle {
    pub path: PathBuf,
    pub label: String,
    pub language: Option<String>,
    /// Whether `to_web_vtt` can turn this format into a text track.
    pub web_playable: bool,
}

/// Subtitle files sitting beside `video` that belong to it: either the exact
/// file stem (`talk.srt`) or the stem followed by a dotted suffix
/// (`talk.en.srt`, `talk.en.forced.srt`), matched case-insensitively.
pub fn sidecar_subtitles(video: &Path) -> Vec<SidecarSubtitle> {
    let (Some(directory), Some(stem)) =
        (video.parent(), video.file_stem().and_then(|s| s.to_str()))
    else {
        return Vec::new();
    };
    let stem = stem.to_lowercase();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };

    let mut found: Vec<(Option<String>, SidecarSubtitle)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let extension = path.extension()?.to_str()?.to_lowercase();
            if !SUBTITLE_EXTENSIONS.contains(&extension.as_str()) {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_lowercase();
            let suffix = match name.strip_prefix(&stem) {
                Some("") => None,
                // Without the dot, `talk-2.srt` would be read as a sidecar of `talk`.
                Some(rest) => Some(rest.strip_prefix('.')?.to_owned()),
                None => return None,
            };
            Some((
                suffix.clone(),
                SidecarSubtitle {
                    label: label_for(suffix.as_deref()),
                    language: suffix.as_deref().and_then(language_code),
                    web_playable: matches!(extension.as_str(), "srt" | "vtt"),
                    path,
                },
            ))
        })
        .collect();

    // The unsuffixed file is the video's default subtitle, so it leads.
    found.sort_by(|(left, first), (right, second)| {
        left.is_some()
            .cmp(&right.is_some())
            .then_with(|| first.label.cmp(&second.label))
    });
    found.into_iter().map(|(_, subtitle)| subtitle).collect()
}

fn label_for(suffix: Option<&str>) -> String {
    let Some(suffix) = suffix else {
        return "Subtitles".to_owned();
    };
    let words: Vec<String> = suffix
        .split('.')
        .filter(|part| !part.is_empty())
        .map(|part| {
            if language_code(part).is_some() {
                part.to_uppercase()
            } else {
                let mut characters = part.chars();
                match characters.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect();
    if words.is_empty() {
        "Subtitles".to_owned()
    } else {
        words.join(" ")
    }
}

fn language_code(suffix: &str) -> Option<String> {
    let code = suffix.split('.').next()?;
    let is_code = (2..=3).contains(&code.len()) && code.chars().all(|c| c.is_ascii_alphabetic());
    is_code.then(|| code.to_lowercase())
}
