//! Telegram backend: `sendMessage` via the Bot API. The bot token lives in the
//! URL path, so it's held as a [`Secret`] and only exposed when the request URL
//! is built; [`super::post_json`] strips the URL from any error so the token
//! never lands in a log.

use serde::Serialize;

use super::{Notifier, NotifyError, RenderedMessage};
use crate::config::Secret;

/// Production Telegram Bot API host. Overridable in tests so a mock server can
/// stand in (mirrors `OciRegistry::with_base_url`).
const DEFAULT_BASE_URL: &str = "https://api.telegram.org";

pub struct TelegramNotifier {
    name: String,
    bot_token: Secret,
    chat_id: String,
    base_url: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct Payload<'a> {
    chat_id: &'a str,
    text: String,
}

/// Telegram has no title field, so title and body are sent as one text block.
fn text(msg: &RenderedMessage) -> String {
    format!("{}\n\n{}", msg.title, msg.body)
}

impl TelegramNotifier {
    pub fn new(
        name: impl Into<String>,
        bot_token: Secret,
        chat_id: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            name: name.into(),
            bot_token,
            chat_id: chat_id.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            client,
        }
    }

    /// Test seam: route at a mock server instead of api.telegram.org. A plain
    /// `pub fn` (not `#[cfg(test)]`) because integration tests under `tests/`
    /// link the library without `cfg(test)` — same pattern as
    /// `OciRegistry::with_base_url`.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

#[async_trait::async_trait]
impl Notifier for TelegramNotifier {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(&self, msg: &RenderedMessage) -> Result<(), NotifyError> {
        let url = format!(
            "{}/bot{}/sendMessage",
            self.base_url,
            self.bot_token.expose()
        );
        let body = Payload {
            chat_id: &self.chat_id,
            text: text(msg),
        };
        super::post_json(&self.client, &url, &body).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::NotifyEvent;

    #[test]
    fn text_joins_title_and_body() {
        let msg = NotifyEvent::UpdateAvailable {
            container: "web".into(),
            image: "nginx:latest".into(),
            latest_digest: "sha256:abc".into(),
        }
        .render();
        let t = text(&msg);
        assert!(t.starts_with(&msg.title));
        assert!(t.ends_with(&msg.body));
        assert!(t.contains("\n\n"));
    }

    #[test]
    fn payload_serializes_chat_id_and_text() {
        let value = serde_json::to_value(Payload {
            chat_id: "12345",
            text: "hello".into(),
        })
        .unwrap();
        assert_eq!(value["chat_id"], serde_json::json!("12345"));
        assert_eq!(value["text"], serde_json::json!("hello"));
    }
}
