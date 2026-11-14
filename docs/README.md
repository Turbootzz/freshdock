# freshdock documentation

freshdock is a health-gated Docker container auto-updater in a single Rust binary, driven by
container labels and environment variables. New here? Start with the
[quickstart](quickstart.md).

The [freshdock.dev](https://freshdock.dev) site covers why freshdock exists and how it
[compares with Watchtower](https://freshdock.dev/watchtower-alternative); this book covers
how it works, down to every label, flag, and environment variable.

## Getting started

- [**Quickstart**](quickstart.md): opt a container in and run the daemon in about a minute.
- [**Coming from Watchtower?**](migrating-from-watchtower.md): label and flag translation.
- [**Troubleshooting**](troubleshooting.md): symptom-first fixes for common first-run issues.

## Reference

- [**Configuration**](configuration.md): the single source of truth for labels, environment variables (the primary path), and the optional `freshdock.toml`.
- [**CLI reference**](cli-reference.md): `check`, `recreate`, `run`, and every flag.
- [**Scheduling & modes**](scheduling.md): update modes and cron syntax.
- [**Compose projects**](compose.md): a Compose stack updated as one unit, with one-shot migrations and `depends_on` ordering.
- [**Notifications**](notifications.md): webhook, Discord, Telegram, SMTP.
- [**Health gating & rollback**](health-and-rollback.md): the recreate lifecycle and image cleanup.
- [**Lifecycle hooks**](lifecycle-hooks.md): pre-update and post-update commands.
- [**Registry authentication**](registry-auth.md): private registries and credentials.
- [**Deployment**](deployment.md): container, systemd, socket permissions, compatibility.

## Project & process

- [Project README](https://github.com/Turbootzz/freshdock/blob/main/README.md): overview, install options, capability summary.
- [Architecture & roadmap](PLAN.md): design, phases, goals, risks.
- [Release runbook](https://github.com/Turbootzz/freshdock/blob/main/RELEASE.md): how a release is cut.
- [Changelog](https://github.com/Turbootzz/freshdock/blob/main/CHANGELOG.md)
- [Contributing](https://github.com/Turbootzz/freshdock/blob/main/CONTRIBUTING.md)
- [Manual test playbooks](https://github.com/Turbootzz/freshdock/tree/main/docs/manual-tests): maintainer smoke tests.

Runnable example stacks: [`examples/compose/`](https://github.com/Turbootzz/freshdock/tree/main/examples/compose).
