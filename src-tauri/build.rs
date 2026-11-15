fn main() {
    // The CLI embeds the git-derived build version (see `cli::version_text`).
    println!("cargo:rerun-if-env-changed=VITE_APP_VERSION");
    tauri_build::build()
}
