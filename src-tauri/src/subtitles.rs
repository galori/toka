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
    let (Some(directory), Some(stem)) = (video.parent(), video.file_stem().and_then(|s| s.to_str()))
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

/// WebVTT for the web media engine, or `None` for formats it cannot render.
pub fn to_web_vtt(source: &str, extension: &str) -> Option<String> {
    let text = source
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    match extension.to_lowercase().as_str() {
        "vtt" if text.trim_start().starts_with("WEBVTT") => Some(text),
        "vtt" => Some(format!("WEBVTT\n\n{text}")),
        "srt" => Some(format!("WEBVTT\n\n{}", srt_timings_to_vtt(&text))),
        _ => None,
    }
}

/// SRT separates milliseconds with a comma and WebVTT with a period. Only cue
/// timing lines are rewritten, so commas inside dialogue survive.
fn srt_timings_to_vtt(text: &str) -> String {
    let mut converted = text
        .lines()
        .map(|line| {
            if line.contains("-->") {
                line.replace(',', ".")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.ends_with('\n') {
        converted.push('\n');
    }
    converted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(directory: &Path, name: &str) {
        fs::write(directory.join(name), "").unwrap();
    }

    #[test]
    fn collects_the_exact_and_language_suffixed_sidecars_for_a_video() {
        let directory = tempfile::tempdir().unwrap();
        write(directory.path(), "talk.mp4");
        write(directory.path(), "talk.srt");
        write(directory.path(), "talk.en.vtt");
        write(directory.path(), "talk.pt-br.srt");

        let found = sidecar_subtitles(&directory.path().join("talk.mp4"));

        assert_eq!(
            found
                .iter()
                .map(|subtitle| subtitle.label.as_str())
                .collect::<Vec<_>>(),
            ["Subtitles", "EN", "Pt-br"]
        );
        assert_eq!(found[1].language.as_deref(), Some("en"));
        assert_eq!(found[0].language, None);
    }

    #[test]
    fn ignores_unrelated_files_and_other_videos_subtitles() {
        let directory = tempfile::tempdir().unwrap();
        write(directory.path(), "talk.mp4");
        write(directory.path(), "talk.srt");
        write(directory.path(), "talk-2.srt");
        write(directory.path(), "another.srt");
        write(directory.path(), "talk.txt");
        write(directory.path(), "talk.en.jpg");

        let found = sidecar_subtitles(&directory.path().join("talk.mp4"));

        assert_eq!(
            found
                .iter()
                .map(|subtitle| subtitle.path.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            ["talk.srt"]
        );
    }

    #[test]
    fn matches_sidecars_whose_case_differs_from_the_video() {
        let directory = tempfile::tempdir().unwrap();
        write(directory.path(), "Talk.mp4");
        write(directory.path(), "TALK.EN.SRT");

        let found = sidecar_subtitles(&directory.path().join("Talk.mp4"));

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label, "EN");
    }

    #[test]
    fn reports_which_sidecar_formats_the_web_engine_can_render() {
        let directory = tempfile::tempdir().unwrap();
        write(directory.path(), "talk.mp4");
        write(directory.path(), "talk.srt");
        write(directory.path(), "talk.styled.ass");

        let found = sidecar_subtitles(&directory.path().join("talk.mp4"));

        assert!(found[0].web_playable);
        assert!(!found[1].web_playable);
    }

    #[test]
    fn returns_no_sidecars_when_the_folder_cannot_be_read() {
        assert_eq!(
            sidecar_subtitles(Path::new("/definitely/not/a/folder/talk.mp4")),
            Vec::new()
        );
    }

    #[test]
    fn converts_srt_timings_without_touching_dialogue_commas() {
        let vtt = to_web_vtt(
            "1\r\n00:00:01,000 --> 00:00:04,500\r\nWell, hello there\r\n",
            "srt",
        )
        .unwrap();

        assert_eq!(
            vtt,
            "WEBVTT\n\n1\n00:00:01.000 --> 00:00:04.500\nWell, hello there\n"
        );
    }

    #[test]
    fn keeps_webvtt_as_it_is_but_adds_a_missing_header() {
        assert!(to_web_vtt("WEBVTT\n\n00:01.000 --> 00:02.000\nHi", "vtt")
            .unwrap()
            .starts_with("WEBVTT"));
        assert_eq!(
            to_web_vtt("\u{feff}00:01.000 --> 00:02.000\nHi", "VTT").unwrap(),
            "WEBVTT\n\n00:01.000 --> 00:02.000\nHi"
        );
    }

    #[test]
    fn refuses_formats_the_web_engine_cannot_render() {
        assert_eq!(to_web_vtt("[Script Info]", "ass"), None);
        assert_eq!(to_web_vtt("anything", "sub"), None);
    }
}
