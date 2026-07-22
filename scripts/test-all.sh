#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd -P)
cd "$project_dir"

persistent_development=/live/persistence/TailsData_unlocked/Persistent/development
if [ -d "$persistent_development/rustup" ]; then
    export RUSTUP_HOME="$persistent_development/rustup"
fi
if [ -d "$persistent_development/cargo-home" ]; then
    export CARGO_HOME="$persistent_development/cargo-home"
fi

printf '\n==> Frontend unit tests\n'
npm test

printf '\n==> Rust unit tests\n'
cargo test --manifest-path src-tauri/Cargo.toml

printf '\n==> Fixture-backed native integration tests\n'
npm run test:integration

printf '\n==> Live-provider native integration tests\n'
npm run test:integration:live

printf '\nAll test suites passed.\n'
