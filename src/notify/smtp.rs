//! SMTP backend (lettre). A different transport from the HTTP backends —
//! separate crate, separate config — but it consumes the same
//! [`RenderedMessage`]: `title` → Subject, `body` → the plain-text part.
//!
//! Message construction is split into the free [`build_message`] so it can be
//! unit-tested without a transport or a relay; CI cannot reach a real SMTP
//! server, so transport behaviour is covered by the manual test in
//! `docs/manual-tests/smtp.md`.

use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use super::{Notifier, NotifyError, RenderedMessage};
use crate::config::Secret;

pub struct SmtpNotifier {
    name: String,
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    to: Vec<Mailbox>,
}

/// Parameters for one SMTP target, already pulled out of config (with the
/// password env-overlay applied). Grouped to keep `new` from sprouting a long
/// positional argument list.
pub struct SmtpParams {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<Secret>,
    pub from: String,
    pub to: Vec<String>,
    /// STARTTLS on the submission port (587) vs implicit TLS (465).
    pub starttls: bool,
}

/// Parse one address, attributing a failure to the named target.
fn mailbox(name: &str, addr: &str) -> Result<Mailbox, NotifyError> {
    addr.parse::<Mailbox>().map_err(|e| NotifyError::Config {
        name: name.to_string(),
        reason: format!("invalid email address `{addr}`: {e}"),
    })
}

/// Build the email from the rendered message. Pure (no transport) so it's unit
/// testable: `title` → Subject, `body` → the text part.
fn build_message(
    from: &Mailbox,
    to: &[Mailbox],
    msg: &RenderedMessage,
) -> Result<Message, NotifyError> {
    let mut builder = Message::builder()
        .from(from.clone())
        .subject(msg.title.as_str());
    for addr in to {
        builder = builder.to(addr.clone());
    }
    builder
        .body(msg.body.clone())
        .map_err(|e| NotifyError::Smtp(e.to_string()))
}

impl SmtpNotifier {
    pub fn new(params: SmtpParams) -> Result<Self, NotifyError> {
        let from = mailbox(&params.name, &params.from)?;
        let to = params
            .to
            .iter()
            .map(|addr| mailbox(&params.name, addr))
            .collect::<Result<Vec<_>, _>>()?;
        if to.is_empty() {
            return Err(NotifyError::Config {
                name: params.name.clone(),
                reason: "smtp target has no `to` recipients".to_string(),
            });
        }

        let relay = if params.starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&params.host)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&params.host)
        }
        .map_err(|e| NotifyError::Config {
            name: params.name.clone(),
            reason: format!("invalid smtp relay `{}`: {e}", params.host),
        })?;

        let mut builder = relay.port(params.port);
        if let (Some(user), Some(pass)) = (params.username, params.password) {
            builder = builder.credentials(Credentials::new(user, pass.expose().to_string()));
        }

        Ok(Self {
            name: params.name,
            transport: builder.build(),
            from,
            to,
        })
    }
}

#[async_trait::async_trait]
impl Notifier for SmtpNotifier {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(&self, msg: &RenderedMessage) -> Result<(), NotifyError> {
        let email = build_message(&self.from, &self.to, msg)?;
        self.transport
            .send(email)
            .await
            .map_err(|e| NotifyError::Smtp(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::NotifyEvent;

    fn rendered() -> RenderedMessage {
        NotifyEvent::UpdateSucceeded {
            container: "web".into(),
            image: "nginx:latest".into(),
            new_id: "sha256:abc".into(),
        }
        .render()
    }

    #[test]
    fn build_message_sets_headers_and_body() {
        let from: Mailbox = "freshdock@example.com".parse().unwrap();
        let to: Vec<Mailbox> = vec!["admin@example.com".parse().unwrap()];
        let msg = rendered();
        let email = build_message(&from, &to, &msg).unwrap();
        let raw = String::from_utf8(email.formatted()).unwrap();
        assert!(raw.contains("From: freshdock@example.com"));
        assert!(raw.contains("To: admin@example.com"));
        assert!(raw.contains(&format!("Subject: {}", msg.title)));
        assert!(raw.contains("passed its health check"));
    }

    #[test]
    fn new_rejects_a_bad_from_address() {
        // `matches!` on the Result avoids requiring Debug on the Ok type
        // (SmtpNotifier wraps a non-Debug transport).
        let result = SmtpNotifier::new(SmtpParams {
            name: "email".into(),
            host: "smtp.example.com".into(),
            port: 587,
            username: None,
            password: None,
            from: "not-an-email".into(),
            to: vec!["admin@example.com".into()],
            starttls: true,
        });
        assert!(matches!(result, Err(NotifyError::Config { .. })));
    }

    #[test]
    fn new_rejects_empty_recipients() {
        let result = SmtpNotifier::new(SmtpParams {
            name: "email".into(),
            host: "smtp.example.com".into(),
            port: 587,
            username: None,
            password: None,
            from: "freshdock@example.com".into(),
            to: vec![],
            starttls: true,
        });
        assert!(matches!(result, Err(NotifyError::Config { .. })));
    }

    #[test]
    fn new_rejects_one_bad_recipient_among_valid_ones() {
        let result = SmtpNotifier::new(SmtpParams {
            name: "email".into(),
            host: "smtp.example.com".into(),
            port: 587,
            username: None,
            password: None,
            from: "freshdock@example.com".into(),
            to: vec!["ok@example.com".into(), "not-an-email".into()],
            starttls: true,
        });
        assert!(matches!(result, Err(NotifyError::Config { .. })));
    }

    #[test]
    fn build_message_lists_every_recipient() {
        let from: Mailbox = "freshdock@example.com".parse().unwrap();
        let to: Vec<Mailbox> = vec![
            "a@example.com".parse().unwrap(),
            "b@example.com".parse().unwrap(),
        ];
        let raw =
            String::from_utf8(build_message(&from, &to, &rendered()).unwrap().formatted()).unwrap();
        assert!(raw.contains("a@example.com"));
        assert!(raw.contains("b@example.com"));
    }
}
