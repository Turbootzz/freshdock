# Contributing to freshdock

Thanks for your interest! freshdock is built phase-by-phase per
[docs/PLAN.md](docs/PLAN.md) — please read it (especially the goals and **non-goals**
in §3) before proposing scope changes.

## Ways to help

- File issues: bugs, with your freshdock version, Docker version, and the container
  labels involved.
- Feedback on [docs/PLAN.md](docs/PLAN.md): registries, notification targets, or
  label conventions you'd want supported.
- Tell us what broke for you in Watchtower or its forks — pain points are the best
  feature requests.

## Local development

The [`justfile`](justfile) is the source of truth for every command; CI and the
tracked pre-push hook both delegate to it.

One-time setup after cloning:

```bash
cargo install just cargo-deny   # if you don't already have them
just install-hooks              # enables .githooks/pre-push
```

The Rust toolchain is **pinned** in [rust-toolchain.toml](rust-toolchain.toml) so
local hooks and CI run on the exact same compiler and clippy ruleset.

### The quality gate

```bash
just ci      # fmt-check + clippy + test + deny — run this before claiming done
```

| Recipe | What it runs |
|---|---|
| `just fmt` / `just fmt-check` | apply / verify formatting |
| `just clippy` | `clippy --all-targets --all-features --locked -- -D warnings` (warnings = errors) |
| `just test` | `cargo test --locked --all-features` |
| `just deny` | license / advisory / dependency check via `cargo-deny` |
| `just build` | release build |

The pre-push hook delegates to `just ci`, so anything that would fail on GitHub fails
locally first. Bypass with `git push --no-verify` for WIP branches.

Single-test runs use cargo directly, e.g. `cargo test --locked --lib labels::tests`
or `cargo test --locked --test registry_mock`.

### Release-blocker quality gate

The live "weird config" recreate round-trip
([tests/recreate_roundtrip_live.rs](tests/recreate_roundtrip_live.rs), PLAN §6.3)
creates a kitchen-sink container, recreates it against a real daemon, and asserts the
inspected config round-trips byte-identical. It is `#[ignore]`d (needs Docker) so
default `cargo test` stays green; CI runs it in a dedicated job, and a failure blocks
release. Run it locally with:

```bash
cargo test --test recreate_roundtrip_live -- --ignored
```

## Dependency hygiene

[deny.toml](deny.toml) is strict and enforced in CI:

- **License allowlist** — Apache-2.0, MIT, BSD-3-Clause, Unicode-3.0, ISC,
  CDLA-Permissive-2.0, 0BSD. Anything else fails.
- `multiple-versions = "deny"` — resolve duplicate transitive versions before merging.
- `wildcards = "deny"` and `yanked = "deny"`.

Adding a dependency means: pick a license-clean crate, run `just deny`, and resolve
any duplicate-version warnings. New dependencies should be justified against PLAN's
"less dependency surface" principle.

## Conventions

- **Comments explain _why_, not _what_** — document non-obvious intent and gotchas,
  not what the code already says. One tight sentence beats a paragraph.
- Errors use `thiserror` enums in libraries; the binary collapses to `anyhow`.
- Match the surrounding code's naming, idioms, and comment density.

## Maintainers

Cutting a release is documented in [RELEASE.md](RELEASE.md).
