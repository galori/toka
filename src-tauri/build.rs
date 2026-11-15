use std::fs;

fn main() {
    // The frontend embeds the SHA from `build-info.json` (written by
    // bin/prepare-build.mjs), so read the same file here. A bare `cargo build`
    // after `npm run build:*` then reports the same SHA as the launch screen
    // instead of falling back to "development".
    println!("cargo:rerun-if-changed=build-info.json");
    println!("cargo:rerun-if-env-changed=VITE_GIT_SHA");
    let sha = std::env::var("VITE_GIT_SHA").ok().or_else(|| {
        let text = fs::read_to_string("build-info.json").ok()?;
        // The file is a flat object written by prepare-build.mjs; a SHA has no
        // characters that need JSON escaping, so no parser is needed.
        let rest = text.split("\"gitSha\"").nth(1)?;
        let value = rest.split('"').nth(1)?;
        Some(value.to_owned())
    });
    if let Some(sha) = sha.filter(|sha| !sha.is_empty()) {
        println!("cargo:rustc-env=TOKA_GIT_SHA={sha}");
    }
    tauri_build::build()
}
