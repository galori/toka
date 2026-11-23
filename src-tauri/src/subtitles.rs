#[cfg(test)]
use std::path::Path;

// Sidecar discovery lives in `toka-search` (tag renames move sidecars along),
// re-exported here so the app keeps a single `subtitles` entry point.
pub use toka_search::sidecars::sidecar_subtitles;

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
