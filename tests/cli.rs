//! Binary-level end-to-end tests for the `freshdock` CLI.
//!
//! These run the compiled binary and assert only on paths that resolve *before*
//! the Docker connect — `main` loads config first, so help/version and config
//! errors are deterministic on any machine, with or without a Docker socket.
//! Commands that need a live daemon (`check`/`run` happy paths) are covered
//! library-side by the scheduler tests, not here.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_freshdock"))
        .args(args)
        .output()
        .expect("spawn freshdock binary")
}

/// Write a throwaway config and return its path. Distinct name per test so
/// parallel runs don't collide.
fn temp_config(name: &str, body: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("freshdock-cli-{name}.toml"));
    std::fs::write(&path, body).expect("write temp config");
    path
}

#[test]
fn help_lists_the_subcommands() {
    let out = run(&["--help"]);
    assert!(out.status.success(), "--help should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("check"), "help missing `check`: {stdout}");
    assert!(
        stdout.contains("recreate"),
        "help missing `recreate`: {stdout}"
    );
    assert!(stdout.contains("run"), "help missing `run`: {stdout}");
}

#[test]
fn version_exits_zero() {
    let out = run(&["--version"]);
    assert!(out.status.success(), "--version should exit 0");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("freshdock"),
        "version output should name the binary"
    );
}

#[test]
fn malformed_config_fails_at_parse_before_docker() {
    let cfg = temp_config("malformed", "[unterminated\n");
    let out = run(&["--config", cfg.to_str().unwrap(), "check"]);
    assert!(
        !out.status.success(),
        "malformed config must be a hard error"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("parsing config file"),
        "expected a parse error, got: {stderr}"
    );
    let _ = std::fs::remove_file(&cfg);
}

#[test]
fn explicit_missing_config_path_is_an_error() {
    let out = run(&["--config", "/no/such/dir/freshdock.toml", "check"]);
    assert!(!out.status.success(), "an explicit missing path must error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("reading config file"),
        "expected a read error, got: {stderr}"
    );
}

#[test]
fn unknown_notification_type_is_a_config_error() {
    let cfg = temp_config(
        "bad-notify",
        "[notifications.x]\ntype = \"bogus\"\nurl = \"https://example.com\"\n",
    );
    let out = run(&["--config", cfg.to_str().unwrap(), "check"]);
    assert!(
        !out.status.success(),
        "an unknown notification type must fail at parse"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("parsing config file"),
        "expected a parse error, got: {stderr}"
    );
    let _ = std::fs::remove_file(&cfg);
}
