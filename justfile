# `just` with no arguments lists every recipe.
default:
    @just --list

# Run the full local CI suite (mirrors .github/workflows/ci.yml minus cross-compile).
ci: fmt-check clippy test deny

# Verify formatting without modifying files.
fmt-check:
    cargo fmt --all -- --check

# Apply formatting in place.
fmt:
    cargo fmt --all

# Lint with warnings escalated to errors.
clippy:
    cargo clippy --all-targets --all-features --locked -- -D warnings

# Run unit + integration tests.
test:
    cargo test --locked --all-features

# Check licenses, advisories, and bans (requires `cargo install cargo-deny`).
deny:
    cargo deny check

# Release build.
build:
    cargo build --release --locked

# Rehearse the crates.io publish exactly as CI does, without uploading anything.
# Run this before tagging a release (see RELEASE.md).
release-dry-run:
    cargo publish --dry-run --locked
    cargo package --list

# One-time per-clone setup: enable the tracked pre-push hook.
install-hooks:
    git config core.hooksPath .githooks
    @echo "Pre-push hook enabled. Disable with: git config --unset core.hooksPath"
