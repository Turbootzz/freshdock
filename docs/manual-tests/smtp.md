# Manual smoke test: SMTP notifications

The SMTP backend talks a real mail protocol, so CI can't exercise the TLS modes
without a relay. Message construction is covered automatically by the
`build_message` unit tests in [src/notify/smtp.rs](https://github.com/Turbootzz/freshdock/blob/main/src/notify/smtp.rs), and plaintext delivery by
[tests/smtp_plaintext.rs](https://github.com/Turbootzz/freshdock/blob/main/tests/smtp_plaintext.rs) against an in-process fake server; this procedure
verifies the transport (connection, STARTTLS, auth) against a real catcher.

## Prerequisites

- `freshdock` built locally: `just build`.
- A local SMTP catcher. [mailpit](https://github.com/axllent/mailpit) is the
  simplest — it exposes SMTP on `:1025` and a web inbox on `:8025`:

  ```bash
  docker run --rm -p 1025:1025 -p 8025:8025 axllent/mailpit
  ```

## Plain delivery (no TLS, no auth)

1. Write a `freshdock.toml` pointing at the catcher. `tls = "none"` because
   mailpit's default listener speaks plaintext SMTP:

   ```toml
   [notifications.email]
   type = "smtp"
   host = "localhost"
   port = 1025
   from = "freshdock@example.com"
   to = ["admin@example.com"]
   tls = "none"
   # triggers omitted → subscribes to available, succeeded, and failed
   ```

   Or skip the file entirely and declare the same target with an
   [env URL](../notifications.md#declaring-targets-from-the-environment):

   ```bash
   export FRESHDOCK_NOTIFY_EMAIL_URL='smtp://localhost:1025/?from=freshdock@example.com&to=admin@example.com&tls=none'
   ```

   Not `starttls = false`: that legacy key selects **implicit TLS** (SMTPS), so
   the earlier version of these instructions opened a TLS handshake against
   mailpit's plaintext listener and could never deliver (#57/#58). Only
   `tls = "none"` disables encryption.

2. Trigger a notification. The quickest path is a watch-mode container with a
   newer image available; or force a failed update (a broken healthcheck) to
   exercise the `failed` trigger and its rollback detail. Run the daemon:

   ```bash
   cargo run -- run
   ```

   Startup logs one WARN per plaintext target (`smtp target uses a PLAINTEXT
   transport …`) — expected here, and the reason `tls = "none"` never belongs in
   a deployment.

3. Open the mailpit inbox at <http://localhost:8025> and confirm a message
   arrived with the rendered **Subject** (`Update available: …` /
   `Updated: …` / `Update failed: …`) and the matching body.

## STARTTLS + auth

STARTTLS and PLAIN/LOGIN auth must be verified against a server that requires
them (mailpit's `--smtp-auth` modes, a real provider, or
[smtp4dev](https://github.com/rnwood/smtp4dev) with TLS enabled):

1. Point `host`/`port` at the TLS-capable relay, set `tls = "starttls"`, and add
   credentials. The password may be supplied inline or via the environment
   override (so it stays out of the file):

   ```toml
   [notifications.email]
   type = "smtp"
   host = "smtp.example.com"
   port = 587
   username = "freshdock@example.com"
   from = "freshdock@example.com"
   to = ["admin@example.com"]
   tls = "starttls"
   ```

   ```bash
   export FRESHDOCK_NOTIFY_EMAIL_PASSWORD='app-password'
   cargo run -- run
   ```

   Or as a single env URL (percent-encode `@`/`:` in the login as `%40`/`%3A`):

   ```bash
   export FRESHDOCK_NOTIFY_EMAIL_URL='smtp://freshdock%40example.com:app-password@smtp.example.com:587/?from=freshdock@example.com&to=admin@example.com&tls=starttls'
   ```

2. Confirm the message is delivered. A STARTTLS handshake failure surfaces as a
   `smtp send failed: …` WARN line; delivery is non-fatal, so the daemon keeps
   running regardless.

## Pass criteria

- A message with the correct subject/body lands in the inbox for each trigger.
- The STARTTLS run authenticates and delivers without falling back to plaintext.
- The bot token / SMTP password never appears in `freshdock`'s log output.
