//! Generic webhook backend: POST the rendered message as a small, stable JSON
//! object. The shape is deliberately minimal (no templating — KISS, PLAN §5.4)
//! so a receiver can route on `event`/`container` without parsing prose.

use serde::Serialize;

use super::{Notifier, NotifyError, RenderedMessage};

pub struct WebhookNotifier {
    name: String,
    url: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct Payload<'a> {
    event: &'a str,
    container: &'a str,
    title: &'a str,
    body: &'a str,
}

fn payload(msg: &RenderedMessage) -> Payload<'_> {
    Payload {
        event: msg.trigger.as_str(),
        container: &msg.container,
        title: &msg.title,
        body: &msg.body,
    }
}

impl WebhookNotifier {
    pub fn new(name: impl Into<String>, url: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            client,
        }
    }
}

#[async_trait::async_trait]
impl Notifier for WebhookNotifier {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(&self, msg: &RenderedMessage) -> Result<(), NotifyError> {
        super::post_json(&self.client, &self.url, &payload(msg)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{NotifyEvent, Trigger};
    use serde_json::json;

    #[test]
    fn payload_carries_event_container_and_text() {
        let msg = NotifyEvent::UpdateSucceeded {
            container: "web".into(),
            image: "nginx:latest".into(),
            new_id: "sha256:deadbeef".into(),
        }
        .render();
        let value = serde_json::to_value(payload(&msg)).unwrap();
        assert_eq!(value["event"], json!(Trigger::Succeeded.as_str()));
        assert_eq!(value["container"], json!("web"));
        assert_eq!(value["title"], json!(msg.title));
        assert_eq!(value["body"], json!(msg.body));
    }
}
