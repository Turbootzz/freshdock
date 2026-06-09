# Notifications

The scheduler ([`freshdock run`](cli-reference.md#freshdock-run)) can notify you when
an opted-in container (`freshdock.notify=true`) reaches one of three events. Targets
are configured as [`[notifications.<name>]`](configuration.md#notificationsname)
tables in `freshdock.toml`.

## Events (triggers)

| Trigger | When | Applies to modes |
|---|---|---|
| `available` | A newer image exists but was **not** applied. | `watch` |
| `succeeded` | A recreate passed its [health gate](health-and-rollback.md). | `live` / `nightly` / `weekly` / `monthly` |
| `failed` | A recreate failed health and was **rolled back**. | `live` / `nightly` / `weekly` / `monthly` |

Each target may subscribe to a subset with `triggers = [...]`; omitting it (or `[]`)
subscribes to all three. The failure message includes the reason (health-check
timeout, or the container crashed before becoming healthy).

Delivery is **best-effort and non-fatal**: a send that fails is logged
(`notification failed; continuing`) and skipped. A broken notifier never blocks or
rolls back an update.

## Backends

All backends render from the same message (a `title` and a `body`); only the wire
format differs.

### webhook

A minimal, stable JSON object — `POST`ed so a receiver can route on `event` /
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

A single embed whose left-bar colour encodes severity — amber for `available`,
green for `succeeded`, red for `failed` — with the title and body as the embed text.

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

An email with the message title as the subject. Defaults to STARTTLS on port 587;
set `starttls = false` for implicit TLS (typically 465). `username` and `password`
must be set together, or both omitted for an anonymous relay.

```toml
[notifications.email]
type     = "smtp"
host     = "smtp.example.com"
port     = 587
username = "freshdock@example.com"
password = "s3cr3t"              # or FRESHDOCK_NOTIFY_EMAIL_PASSWORD
from     = "freshdock@example.com"
to       = ["admin@example.com"]
starttls = true
triggers = ["failed"]
```

See the [SMTP smoke-test playbook](manual-tests/smtp.md) to verify delivery against
a local catcher.

## Secrets

Webhook URLs, Discord webhook URLs, Telegram bot tokens, and SMTP passwords are
treated as secrets and redacted in all logs. Two can also come from the environment
instead of the file:

- `FRESHDOCK_NOTIFY_<NAME>_BOT_TOKEN` — a Telegram target's `bot_token`
- `FRESHDOCK_NOTIFY_<NAME>_PASSWORD` — an SMTP target's `password`

See the full [environment-variable table](configuration.md#environment-variables).
