# Security policy

## Supported versions

freshdock is pre-1.0. Security fixes target the **latest released version** and
`main`. Once `1.0.0` ships, the latest `1.x` release is supported.

| Version | Supported |
|---|---|
| latest release / `main` | Yes |
| older pre-releases | No |

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Report privately via GitHub's
[**Report a vulnerability**](https://github.com/Turbootzz/freshdock/security/advisories/new)
button (Security, then Advisories), or by email to **thijs@bendy.nl**.

Please include:

- the version (`freshdock --version`) and how you run it (binary, container, compose);
- a description of the issue and its impact;
- reproduction steps or a proof of concept, if you have one.

You'll get an acknowledgement as soon as possible. Once a fix is available we'll
coordinate disclosure and credit you in the release notes unless you prefer to remain
anonymous.

## Scope notes

freshdock talks to the Docker socket, which is effectively root on the host. Grant
that access deliberately (see [deployment](docs/deployment.md#docker-socket-permissions)).
Registry tokens and notification secrets are redacted in logs (even at
`RUST_LOG=trace`); a leak of a secret into log output is in scope.
