# freshdock documentation

Everything beyond the [project README](https://github.com/Turbootzz/freshdock/blob/main/README.md). Start with the quickstart, then
dive into the reference pages.

## Getting started

- [**Quickstart**](quickstart.md) — opt a container in and run the daemon in ~1 minute.
- [**Coming from Watchtower?**](migrating-from-watchtower.md) — label/flag translation.
- [**Troubleshooting**](troubleshooting.md) — symptom-first fixes for common first-run issues.

## Reference

- [**Configuration**](configuration.md) — the single source of truth: labels, environment variables (the primary path), and the optional `freshdock.toml`.
- [**CLI reference**](cli-reference.md) — `check`, `recreate`, `run`, and every flag.
- [**Scheduling & modes**](scheduling.md) — update modes and cron syntax.
- [**Compose projects**](compose.md) — a Compose stack updated as one unit: one-shot migrations, `depends_on` ordering.
- [**Notifications**](notifications.md) — webhook, Discord, Telegram, SMTP.
- [**Health gating & rollback**](health-and-rollback.md) — the recreate lifecycle and image cleanup.
- [**Registry authentication**](registry-auth.md) — private registries and credentials.
- [**Deployment**](deployment.md) — container, systemd, socket permissions, compatibility.

## Project & process

- [Architecture & roadmap](PLAN.md) — design, phases, goals, risks.
- [Release runbook](https://github.com/Turbootzz/freshdock/blob/main/RELEASE.md) — how a release is cut.
- [Changelog](https://github.com/Turbootzz/freshdock/blob/main/CHANGELOG.md)
- [Contributing](https://github.com/Turbootzz/freshdock/blob/main/CONTRIBUTING.md)
- [Manual test playbooks](https://github.com/Turbootzz/freshdock/tree/main/docs/manual-tests) — maintainer smoke tests.

Runnable example stacks: [`examples/compose/`](https://github.com/Turbootzz/freshdock/tree/main/examples/compose).
