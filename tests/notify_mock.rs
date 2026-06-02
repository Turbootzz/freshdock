//! HTTP-mocked tests for the notification backends and the dispatcher.
//!
//! Each HTTP backend is driven against a wiremock server so its on-the-wire
//! shape (path, headers, JSON body) is locked in, plus the dispatcher's
//! trigger routing ("one failed update → exactly one POST per subscribed
//! target, none for others"). SMTP is not HTTP, so it's covered by the
//! `build_message` unit tests + `docs/manual-tests/smtp.md`.

use std::collections::HashMap;

use freshdock::config::{NotificationConfig, NotificationTarget, Secret};
use freshdock::notify::discord::DiscordNotifier;
use freshdock::notify::telegram::TelegramNotifier;
use freshdock::notify::webhook::WebhookNotifier;
use freshdock::notify::{Dispatcher, Notifier, NotifyError, NotifyEvent};
use freshdock::rollback::RollbackReason;
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn succeeded() -> NotifyEvent {
    NotifyEvent::UpdateSucceeded {
        container: "web".into(),
        image: "nginx:latest".into(),
        new_id: "sha256:abc".into(),
    }
}

fn failed() -> NotifyEvent {
    NotifyEvent::UpdateFailed {
        container: "web".into(),
        reason: RollbackReason::HealthTimeout,
        old_image_ref: "nginx:1.0".into(),
        new_image_ref: "nginx:1.1".into(),
        restored_from: "web-old-1".into(),
    }
}

#[tokio::test]
async fn webhook_posts_the_event_as_json() {
    let server = MockServer::start().await;
    let msg = succeeded().render();
    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("content-type", "application/json"))
        .and(body_partial_json(json!({
            "event": "succeeded",
            "container": "web",
            "title": msg.title,
            "body": msg.body,
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    WebhookNotifier::new("hook", server.uri(), freshdock::http::client())
        .send(&msg)
        .await
        .expect("webhook send should succeed");
}

#[tokio::test]
async fn discord_posts_a_single_embed() {
    let server = MockServer::start().await;
    let msg = failed().render();
    Mock::given(method("POST"))
        .and(body_partial_json(json!({
            "embeds": [{ "title": msg.title, "description": msg.body }],
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    DiscordNotifier::new("chat", server.uri(), freshdock::http::client())
        .send(&msg)
        .await
        .expect("discord send should succeed");
}

#[tokio::test]
async fn telegram_calls_send_message_with_chat_id_and_text() {
    let server = MockServer::start().await;
    let msg = succeeded().render();
    Mock::given(method("POST"))
        // The bot token lives in the path; the mock confirms it's placed there.
        .and(path("/bot123:ABC/sendMessage"))
        .and(body_partial_json(json!({ "chat_id": "42" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .expect(1)
        .mount(&server)
        .await;

    TelegramNotifier::new(
        "tg",
        Secret::new("123:ABC"),
        "42",
        freshdock::http::client(),
    )
    .with_base_url(server.uri())
    .send(&msg)
    .await
    .expect("telegram send should succeed");
}

#[tokio::test]
async fn a_non_2xx_response_is_a_typed_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let err = WebhookNotifier::new("hook", server.uri(), freshdock::http::client())
        .send(&succeeded().render())
        .await
        .expect_err("a 500 must be an error");
    assert!(
        matches!(err, NotifyError::Status(s) if s.as_u16() == 500),
        "expected NotifyError::Status(500), got {err:?}"
    );
}

#[tokio::test]
async fn dispatch_hits_each_subscribed_target_once_and_skips_others() {
    let fail_a = MockServer::start().await;
    let fail_b = MockServer::start().await;
    let succ_only = MockServer::start().await;

    // Both failure subscribers must receive exactly one POST for one UpdateFailed.
    for s in [&fail_a, &fail_b] {
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(s)
            .await;
    }
    // The success-only subscriber must receive nothing.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&succ_only)
        .await;

    let mut targets = HashMap::new();
    targets.insert(
        "a".to_string(),
        NotificationTarget::Webhook {
            url: Secret::new(fail_a.uri()),
            triggers: Some(vec!["failed".into()]),
        },
    );
    targets.insert(
        "b".to_string(),
        NotificationTarget::Webhook {
            url: Secret::new(fail_b.uri()),
            triggers: Some(vec!["failed".into()]),
        },
    );
    targets.insert(
        "s".to_string(),
        NotificationTarget::Webhook {
            url: Secret::new(succ_only.uri()),
            triggers: Some(vec!["succeeded".into()]),
        },
    );

    let dispatcher =
        Dispatcher::from_config(NotificationConfig { targets }, freshdock::http::client())
            .expect("dispatcher builds from valid config");
    dispatcher.dispatch(&failed()).await;
    // Per-server .expect(n) is verified when each MockServer drops here.
}
