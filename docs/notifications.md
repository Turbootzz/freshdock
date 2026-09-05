# Notifications

The scheduler ([`freshdock run`](cli-reference.md#freshdock-run)) can notify you when
an opted-in container (`freshdock.notify=true`) reaches one of three events. Configure
a target as a [`[notifications.<name>]`](configuration.md#notificationsname) table in
`freshdock.toml`, or declare it entirely from the environment with a
[shoutrrr-style URL](#declaring-targets-from-the-environment).

## Events (triggers)

| Trigger | When | Applies to modes |
|---|---|---|
| `available` | A newer image exists but was **not** applied. | `watch` |
| `succeeded` | A recreate passed its [health gate](health-and-rollback.md). | `live` / `nightly` / `weekly` / `monthly` |
| `failed` | A recreate failed health and was **rolled back**, or a [Compose rollout](compose.md#when-a-one-shot-fails) aborted on a failed one-shot (one event per project; `container` then names the project). | `live` / `nightly` / `weekly` / `monthly` |

Each target may subscribe to a subset with `triggers = [...]`; omitting it (or `[]`)
subscribes to all three. The failure message includes the reason (health-check
timeout, or the container crashed before becoming healthy).

Delivery is best-effort and non-fatal, so a broken notifier never blocks or rolls
back an update. A successful send logs `notification sent` (with the target, trigger,
and container) at `info`; a send that fails logs `notification failed; continuing` at
`warn` and is skipped. An event that fires with no target subscribed to its trigger is
logged at `debug`, so a missing notification is diagnosable from the logs.

At startup `freshdock run` logs the targets it loaded
(`notification targets configured count=2 targets=["ops(discord)[failed,succeeded]", ...]`),
or `no notification targets configured` when none were found. A typo'd or unset
`FRESHDOCK_NOTIFY_*` variable is therefore visible at boot rather than only when an
update later fails to notify.

## Backends

All backends render from the same message (a `title` and a `body`); only the wire
format differs.

### webhook

A minimal, stable JSON object, `POST`ed so a receiver can route on `event` or
`container` without parsing prose:

```json
{
  "event": "succeeded",
  "container": "web",
  "title": "...",
  "body": "..."
}
```

```toml
[notifications.ops]
type = "webhook"
url  = "https://example.com/hooks/freshdock"
```

### Discord

A single embed whose left-bar colour encodes severity (amber for `available`,
green for `succeeded`, red for `failed`), with the title and body as the embed text.

```toml
[notifications.discord]
type        = "discord"
webhook_url = "https://discord.com/api/webhooks/123/abc"
triggers    = ["succeeded", "failed"]
```

### Telegram

A plain-text message via the Bot API (`sendMessage`).

```toml
[notifications.tg]
type      = "telegram"
bot_token = "123456:ABC-DEF"     # or FRESHDOCK_NOTIFY_TG_BOT_TOKEN
chat_id   = "987654321"
```

### SMTP (email)

An email with the message title as the subject. `username` and `password` must be
set together, or both omitted for an anonymous relay.

`tls` selects the transport security. A mode and a port that don't match can never
complete a handshake, so `port` defaults to whatever the mode conventionally uses:

| `tls` | Meaning | Default `port` |
|---|---|---|
| `"starttls"` | upgrade the connection with STARTTLS (**the default**) | 587 |
| `"implicit"` | TLS from the first byte (SMTPS) | 465 |
| `"none"` | no encryption at all; local catchers and development only | 25 |

Set `port` explicitly whenever the relay listens elsewhere (mailpit, for example,
takes plaintext SMTP on 1025).

```toml
[notifications.email]
type     = "smtp"
host     = "smtp.example.com"
port     = 587                   # optional; 587 is already the starttls default
username = "freshdock@example.com"
password = "s3cr3t"              # or FRESHDOCK_NOTIFY_EMAIL_PASSWORD
from     = "freshdock@example.com"
to       = ["admin@example.com"]
tls      = "starttls"            # default
triggers = ["failed"]
```

`starttls = true|false` is a legacy alias kept for existing configs:

- `true` means `tls = "starttls"`; `false` means `tls = "implicit"`, never plaintext.
- Both keys may be present as long as they agree (`tls = "starttls"` with
  `starttls = true`, or `tls = "implicit"` with `starttls = false`), so adding `tls`
  to an existing config never breaks it.
- A contradictory pair is an error and the target is skipped, naming both values.
  This includes any `tls = "none"` next to a `starttls` key, which the boolean
  cannot express.
- Prefer `tls`, and delete the legacy line once it is there.

`tls = "none"` sends credentials and message content in the clear. freshdock logs a
warning for every plaintext target at startup; use it only against a catcher such as
mailpit on `localhost`.

See the [SMTP smoke-test playbook](manual-tests/smtp.md) to verify delivery against
a local catcher.

## Declaring targets from the environment

Every backend can be declared without a file, using a shoutrrr-style URL (the same
shape Watchtower's `WATCHTOWER_NOTIFICATION_URL` uses), so a container deployment
needs no `freshdock.toml`:

| Env var | Value |
|---|---|
| `FRESHDOCK_NOTIFY_<NAME>_URL` | the target URL (schemes below) |
| `FRESHDOCK_NOTIFY_<NAME>_TRIGGERS` | optional comma list of `available,succeeded,failed`; defaults to all |

`<NAME>` is any label you choose; it appears only in logs. The scheme selects the
backend:

| Scheme | Example | Becomes |
|---|---|---|
| `https` / `http` | `https://example.com/hooks/freshdock` | a generic **webhook** |
| `discord` | `discord://TOKEN@WEBHOOK_ID` | a **Discord** webhook |
| `telegram` | `telegram://BOT_TOKEN@telegram?chats=CHAT_ID` | a **Telegram** message |
| `smtp` | `smtp://user:pass@host:587/?from=ops@x.com&to=a@x.com,b@y.com&tls=starttls` | an **SMTP** email |

```bash
# one Discord target, succeeded + failed only, no file involved
FRESHDOCK_NOTIFY_OPS_URL=discord://abcdef...@123456789
FRESHDOCK_NOTIFY_OPS_TRIGGERS=succeeded,failed
```

Notes:

- A Telegram bot token's `:` (and any `@`/`:` in an SMTP login) are URL
  metacharacters. Percent-encode them in the userinfo (`%40`, `%3A`) and freshdock
  decodes them back. A Telegram token written as `id:secret` is rejoined for you.
- SMTP `?to=` takes a comma list or repeated `to=`; Telegram uses the first
  `chats=` value.
- SMTP `?tls=starttls|implicit|none` mirrors the file key and defaults to
  `starttls`; the legacy `?starttls=true|false` still works, and the two may both
  be present as long as they agree. A contradictory pair is an error and the
  target is skipped.
- An omitted port in an `smtp://` URL follows the same mode-based default as the
  file key (587 / 465 / 25).
- An invalid URL or unknown scheme is warned and skipped, never fatal: the same
  resilience as a malformed file target.
- If a `<NAME>` is already declared in the file, the file target wins and the env
  URL is ignored. Env declaration is purely additive.

## Secrets

Webhook URLs, Discord webhook URLs, Telegram bot tokens, and SMTP passwords are
treated as secrets and redacted in all logs. A secret on a file-declared target can
also come from the environment instead of the file:

- `FRESHDOCK_NOTIFY_<NAME>_BOT_TOKEN`: a Telegram target's `bot_token`
- `FRESHDOCK_NOTIFY_<NAME>_PASSWORD`: an SMTP target's `password`

To declare a whole target, secret included, from the environment, use
[`FRESHDOCK_NOTIFY_<NAME>_URL`](#declaring-targets-from-the-environment).

See the full [environment-variable table](configuration.md#environment-variables).
