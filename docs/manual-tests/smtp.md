# Manual smoke test: SMTP notifications

The SMTP backend talks a real mail protocol, so CI can't exercise it without a
relay. Message construction is covered automatically by the `build_message`
unit tests in [src/notify/smtp.rs](https://github.com/Turbootzz/freshdock/blob/main/src/notify/smtp.rs); this procedure
verifies the transport (connection, STARTTLS, auth) against a local catcher.

## Prerequisites

- `freshdock` built locally: `just build`.
- A local SMTP catcher. [mailpit](https://github.com/axllent/mailpit) is the
  simplest — it exposes SMTP on `:1025` and a web inbox on `:8025`:

  ```bash
  docker run --rm -p 1025:1025 -p 8025:8025 axllent/mailpit
  ```

## Plain delivery (no TLS, no auth)

1. Write a `freshdock.toml` pointing at the catcher. `starttls = false` because
   mailpit's default listener is plaintext:

   ```toml
   [notifications.email]
   type = "smtp"
   host = "localhost"
   port = 1025
   from = "freshdock@example.com"
   to = ["admin@example.com"]
   starttls = false
   # triggers omitted → subscribes to available, succeeded, and failed
   ```

2. Trigger a notification. The quickest path is a watch-mode container with a
   newer image available; or force a failed update (a broken healthcheck) to
   exercise the `failed` trigger and its rollback detail. Run the daemon:

   ```bash
   cargo run -- run
   ```

3. Open the mailpit inbox at <http://localhost:8025> and confirm a message
   arrived with the rendered **Subject** (`Update available: …` /
   `Updated: …` / `Update failed: …`) and the matching body.

## STARTTLS + auth

STARTTLS and PLAIN/LOGIN auth must be verified against a server that requires
them (mailpit's `--smtp-auth` modes, a real provider, or
[smtp4dev](https://github.com/rnwood/smtp4dev) with TLS enabled):

1. Point `host`/`port` at the TLS-capable relay, set `starttls = true`, and add
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
   starttls = true
   ```

   ```bash
   export FRESHDOCK_NOTIFY_EMAIL_PASSWORD='app-password'
   cargo run -- run
   ```

2. Confirm the message is delivered. A STARTTLS handshake failure surfaces as a
   `smtp send failed: …` WARN line; delivery is non-fatal, so the daemon keeps
   running regardless.

## Pass criteria

- A message with the correct subject/body lands in the inbox for each trigger.
- The STARTTLS run authenticates and delivers without falling back to plaintext.
- The bot token / SMTP password never appears in `freshdock`'s log output.
