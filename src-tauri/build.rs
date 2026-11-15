use std::{env, fs, path::Path};

/// A SHA is hex; anything else (empty, whitespace, a stray JSON token) is not
/// provenance and must not reach `cargo:rustc-env`.
fn valid_sha(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.chars().all(|c| c.is_ascii_hexdigit())).then(|| value.to_owned())
}

/// `gitSha` from the flat object `bin/prepare-build.mjs` writes. A SHA needs no
/// JSON escaping, so no parser is needed.
fn sha_from_build_info(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let rest = text.split("\"gitSha\":").nth(1)?;
    valid_sha(rest.trim_start().strip_prefix('"')?.split('"').next()?)
}

fn main() {
    // The CLI embeds the git-derived build version (see `cli::version_text`).
    println!("cargo:rerun-if-env-changed=VITE_APP_VERSION");
    // `build:*` passes VITE_GIT_SHA to the frontend and records the same SHA in
    // build-info.json. A bare `cargo build` afterwards reuses that `dist`, so
    // the file keeps the CLI in step with the launch screen.
    let build_info = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("build-info.json");
    // Only watch a file that exists: Cargo treats a missing watched path as
    // always dirty, which would recompile the crate on every Rust-only build.
    // The cost is that a file created later is not noticed until build.rs
    // reruns for another reason (a `build:*` run sets VITE_GIT_SHA, which does).
    if build_info.exists() {
        println!("cargo:rerun-if-changed={}", build_info.display());
    }
    println!("cargo:rerun-if-env-changed=VITE_GIT_SHA");
    let sha = env::var("VITE_GIT_SHA")
        .ok()
        .and_then(|value| valid_sha(&value))
        .or_else(|| sha_from_build_info(&build_info));
    if let Some(sha) = sha {
        println!("cargo:rustc-env=TOKA_GIT_SHA={sha}");
    }
    tauri_build::build()
}
