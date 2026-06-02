//! Discord backend: post a single embed to a Discord webhook URL. The embed's
//! left-bar colour encodes the trigger (amber/green/red) so the severity reads
//! at a glance; the text itself is the shared rendered message.

use serde::Serialize;

use super::{Notifier, NotifyError, RenderedMessage, Trigger};

pub struct DiscordNotifier {
    name: String,
    webhook_url: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct Embed<'a> {
    title: &'a str,
    description: &'a str,
    color: u32,
}

#[derive(Serialize)]
struct Payload<'a> {
    embeds: [Embed<'a>; 1],
}

/// Discord embed colour (decimal RGB) per trigger severity.
fn color(trigger: Trigger) -> u32 {
    match trigger {
        Trigger::Available => 0x00F1_C40F, // amber — informational
        Trigger::Succeeded => 0x002E_CC71, // green — good
        Trigger::Failed => 0x00E7_4C3C,    // red — bad
    }
}

fn payload(msg: &RenderedMessage) -> Payload<'_> {
    Payload {
        embeds: [Embed {
            title: &msg.title,
            description: &msg.body,
            color: color(msg.trigger),
        }],
    }
}

impl DiscordNotifier {
    pub fn new(
        name: impl Into<String>,
        webhook_url: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            name: name.into(),
            webhook_url: webhook_url.into(),
            client,
        }
    }
}

#[async_trait::async_trait]
impl Notifier for DiscordNotifier {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(&self, msg: &RenderedMessage) -> Result<(), NotifyError> {
        super::post_json(&self.client, &self.webhook_url, &payload(msg)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::NotifyEvent;
    use crate::rollback::RollbackReason;

    #[test]
    fn payload_wraps_text_in_a_single_coloured_embed() {
        let msg = NotifyEvent::UpdateFailed {
            container: "web".into(),
            reason: RollbackReason::Crashed,
            old_image_ref: "nginx:1.0".into(),
            new_image_ref: "nginx:1.1".into(),
            restored_from: "web-old-1".into(),
        }
        .render();
        let value = serde_json::to_value(payload(&msg)).unwrap();
        let embed = &value["embeds"][0];
        assert_eq!(embed["title"], serde_json::json!(msg.title));
        assert_eq!(embed["description"], serde_json::json!(msg.body));
        assert_eq!(embed["color"], serde_json::json!(color(Trigger::Failed)));
        assert_eq!(value["embeds"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn each_trigger_has_a_distinct_color() {
        let colors = [
            color(Trigger::Available),
            color(Trigger::Succeeded),
            color(Trigger::Failed),
        ];
        let unique: std::collections::HashSet<_> = colors.iter().collect();
        assert_eq!(unique.len(), 3, "trigger colours must be distinct");
    }
}
