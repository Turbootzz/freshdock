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

# The live daemon gate, as CI runs it: every `#[ignore]`d test against a real
# daemon. `--test-threads=1` serialises the tests inside each binary (cargo
# already runs one test target at a time); they share one daemon and some move an
# image tag. WARNING: this includes a scheduler tick, which sweeps every
# container on the host labelled `freshdock.enable=true`.
live:
    cargo test --locked --all-features --tests -- --ignored --nocapture --test-threads=1

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

# Build the documentation site into ./book (requires `cargo install mdbook`).
docs:
    mdbook build

# Fail on typographic dashes, arrows, and ellipses in the docs (plain ASCII only).
docs-lint:
    status=0; grep -rnP '[\x{2013}\x{2014}\x{2026}\x{2192}\x{2248}\x{2264}\x{2265}]' README.md CONTRIBUTING.md RELEASE.md SECURITY.md freshdock.toml.example docs examples || status=$?; test "$status" -eq 1

# Serve the docs locally with live reload and open a browser.
docs-serve:
    mdbook serve --open
