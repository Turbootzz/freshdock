# Release runbook

freshdock releases are cut **by a human**. CI builds and stages everything on a
tag push, but the crates.io publish is gated behind the `release` GitHub
environment and **never fires from a tag push alone** — someone has to click
*Approve* in the Actions UI. This is deliberate: a published crate version is
permanent.

The pipeline lives in [`.github/workflows/release.yml`](.github/workflows/release.yml).
On a `v*.*.*` tag it runs:

1. `build` — cross-compile static-musl binaries (amd64, arm64, armv7).
2. `image` — build and push the multi-arch image to `ghcr.io/turbootzz/freshdock`.
3. `package-check` — `cargo publish --dry-run` (fails fast if the crate can't be packaged).
4. `release` — create the GitHub Release and attach the binaries + `SHA256SUMS`.
5. `publish` — **pauses on the `release` environment**; publishes to crates.io only after approval.

## One-time setup (maintainer, MANUAL)

1. Create a [crates.io API token](https://crates.io/settings/tokens) and add it as
   a repository secret named `CARGO_REGISTRY_TOKEN`
   (Settings → Secrets and variables → Actions).
2. Create the approval gate: Settings → Environments → **New environment** named
   `release` → add yourself under **Required reviewers**. Without this, the
   `publish` job would run unattended.

## Pre-flight checklist (every release)

- [ ] `just ci` is green on the release commit.
- [ ] The live "weird config" recreate gate passes:
      `cargo test --test recreate_roundtrip_live -- --ignored`.
- [ ] `just release-dry-run` succeeds.
- [ ] **Community beta sign-off** recorded (PLAN §8, risk row 1): at least one
      external user ran it on a real homelab and reported back. *Required before a
      final `1.0.0`* — not required for an `-rc` candidate.
- [ ] `Cargo.toml` `version` equals the tag you are about to push (the `publish`
      job hard-fails on a mismatch).
- [ ] `CHANGELOG.md` has an entry for this version.
- [ ] `RELEASE_NOTES.md` is updated for this version, including the `blob/<tag>`
      links (they must point at the tag being released).

## Cut the release (MANUAL)

```bash
git tag v1.0.0
git push origin v1.0.0
```

Then in the Actions tab:

1. Watch `build` → `image` → `package-check` → `release` run automatically.
2. The `publish` job stops with a "waiting for review" banner.
3. Review the run, then click **Approve** to publish to crates.io. **This is the
   point of no return — the version number becomes permanent.**

## Verify the acceptance criteria (issue #21)

- [ ] `cargo install freshdock` installs the new version from crates.io.
- [ ] `docker pull ghcr.io/turbootzz/freshdock:<version>` and `:latest` work for
      amd64, arm64, and armv7.
- [ ] The GitHub Release page lists the three binaries and `SHA256SUMS`.
- [ ] Release notes link to `docs/PLAN.md` and the migration guide.

## Recommended: rehearse with a release candidate first

Cut `v1.0.0-rc.1` exactly as above. Because the tag contains a hyphen:

- the image is **not** tagged `:latest`;
- crates.io marks it a pre-release, so `cargo install freshdock` skips it (users
  opt in with `cargo install freshdock --version 1.0.0-rc.1`).

This exercises the whole pipeline — including the approval gate — without burning
the `1.0.0` number. After beta sign-off, bump `Cargo.toml` to `1.0.0`, update the
notes, and tag `v1.0.0`.
