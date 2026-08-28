//! Notifications (Phase 6, PLAN §5.4).
//!
//! One [`Notifier`] trait, four backends (webhook / Discord / Telegram / SMTP),
//! and a [`Dispatcher`] that renders each lifecycle event **once** and fans it
//! out to every target subscribed to that event's [`Trigger`]. A send failure
//! is logged and swallowed — notifications must never abort an update (the
//! scheduler's "a tick never propagates an error" contract).
//!
//! Wording lives in exactly one place ([`NotifyEvent::render`]); each backend
//! only adapts the [`RenderedMessage`] to its wire format, so the three HTTP
//! payloads and the email body can never drift apart (DRY).

pub mod discord;
pub mod smtp;
pub mod telegram;
pub mod webhook;

use std::collections::HashSet;
use std::sync::Arc;

use serde::Serialize;
use tracing::{debug, info, warn};

use crate::config::{NotificationConfig, NotificationTarget, SmtpTls, resolve_smtp_tls};
use crate::format::short_digest;
use crate::rollback::RollbackReason;
use discord::DiscordNotifier;
use smtp::{SmtpNotifier, SmtpParams};
use telegram::TelegramNotifier;
use webhook::WebhookNotifier;

/// Which lifecycle event fired. The config `triggers = [...]` list and the
/// PLAN §5.4 matrix (update available / succeeded / failed) map onto these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// A newer image exists but was not applied (watch mode).
    Available,
    /// A health-gated recreate succeeded.
    Succeeded,
    /// A recreate failed its health gate and was rolled back.
    Failed,
}

impl Trigger {
    /// Canonical lowercase token used in config and the generic webhook payload.
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Available => "available",
            Trigger::Succeeded => "succeeded",
            Trigger::Failed => "failed",
        }
    }

    /// Parse a config token. The error carries the bad token so the caller can
    /// name the offending `[notifications.<name>]` table.
    pub fn parse(token: &str) -> Result<Self, String> {
        match token.trim().to_ascii_lowercase().as_str() {
            "available" => Ok(Trigger::Available),
            "succeeded" => Ok(Trigger::Succeeded),
            "failed" => Ok(Trigger::Failed),
            other => Err(other.to_string()),
        }
    }

    /// Every trigger — the default subscription when a target omits `triggers`.
    pub fn all() -> HashSet<Trigger> {
        [Trigger::Available, Trigger::Succeeded, Trigger::Failed]
            .into_iter()
            .collect()
    }
}

/// A notifiable lifecycle event with everything needed to render every backend.
/// Built at the scheduler's existing log points; `UpdateFailed` mirrors
/// [`crate::rollback::RollbackEvent`] so the rollback detail flows through.
#[derive(Debug, Clone)]
pub enum NotifyEvent {
    UpdateAvailable {
        container: String,
        image: String,
        latest_digest: String,
    },
    UpdateSucceeded {
        container: String,
        image: String,
        new_id: String,
    },
    UpdateFailed {
        container: String,
        reason: RollbackReason,
        old_image_ref: String,
        new_image_ref: String,
        restored_from: String,
    },
    /// A compose project rollout stopped part-way (issue #78). Reported per
    /// *project*, since that is the unit that failed.
    RolloutAborted {
        project: String,
        /// The step that stopped it, already rendered.
        reason: String,
        /// Containers the rollout did update before stopping.
        completed: Vec<String>,
        /// Containers it had planned to update and did not complete.
        remaining: Vec<String>,
    },
}

impl NotifyEvent {
    pub fn trigger(&self) -> Trigger {
        match self {
            NotifyEvent::UpdateAvailable { .. } => Trigger::Available,
            NotifyEvent::UpdateSucceeded { .. } => Trigger::Succeeded,
            NotifyEvent::UpdateFailed { .. } | NotifyEvent::RolloutAborted { .. } => {
                Trigger::Failed
            }
        }
    }

    /// The subject of the event; for a rollout that is the project name.
    pub fn container(&self) -> &str {
        match self {
            NotifyEvent::UpdateAvailable { container, .. }
            | NotifyEvent::UpdateSucceeded { container, .. }
            | NotifyEvent::UpdateFailed { container, .. } => container,
            NotifyEvent::RolloutAborted { project, .. } => project,
        }
    }

    /// The single source of human-readable wording. Backends format the result;
    /// none re-derives the text.
    pub fn render(&self) -> RenderedMessage {
        let (title, body) = match self {
            NotifyEvent::UpdateAvailable {
                container,
                image,
                latest_digest,
            } => (
                format!("Update available: {container}"),
                format!(
                    "A newer image is available for {image} ({}). \
                     Not applied — this container is in watch mode.",
                    short_digest(latest_digest)
                ),
            ),
            NotifyEvent::UpdateSucceeded {
                container,
                image,
                new_id,
            } => (
                format!("Updated: {container}"),
                format!(
                    "{container} was updated to {image} and passed its health check \
                     (new container {}).",
                    short_digest(new_id)
                ),
            ),
            NotifyEvent::UpdateFailed {
                container,
                reason,
                old_image_ref,
                new_image_ref,
                restored_from,
            } => (
                format!("Update failed: {container}"),
                format!(
                    "Updating {container} from {old_image_ref} to {new_image_ref} failed \
                     the health gate ({}); rolled back to the previous container ({restored_from}).",
                    reason_text(*reason)
                ),
            ),
            NotifyEvent::RolloutAborted {
                project,
                reason,
                completed,
                remaining,
            } => (
                format!("Rollout aborted: {project}"),
                format!(
                    "The compose project {project} was rolled out as one unit and stopped \
                     part-way: {reason}. Updated before stopping: {}. Not updated, still \
                     serving the previous image: {}.",
                    name_list(completed),
                    name_list(remaining)
                ),
            ),
        };
        RenderedMessage {
            title,
            body,
            trigger: self.trigger(),
            container: self.container().to_string(),
        }
    }
}

/// Render a container list for a message body, or `none` when it is empty.
fn name_list(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

/// Resolve a target's `triggers` config into a subscription set. Omitted
/// (`None`) subscribes to all three; an unknown token fails with the target
/// named so the operator can find it.
fn parse_triggers(
    name: &str,
    triggers: Option<Vec<String>>,
) -> Result<HashSet<Trigger>, NotifyError> {
    match triggers {
        None => Ok(Trigger::all()),
        Some(list) => list
            .iter()
            .map(|t| {
                Trigger::parse(t).map_err(|bad| NotifyError::Config {
                    name: name.to_string(),
                    reason: format!(
                        "unknown trigger `{bad}` (expected available, succeeded, or failed)"
                    ),
                })
            })
            .collect(),
    }
}

/// The conventional port for each transport mode. Implicit TLS on 587 (or
/// STARTTLS on 465) cannot complete a handshake, so the default has to follow
/// the mode rather than being one fixed number.
fn default_smtp_port(tls: SmtpTls) -> u16 {
    match tls {
        SmtpTls::Starttls => 587,
        SmtpTls::Implicit => 465,
        SmtpTls::Plaintext => 25,
    }
}

/// Resolve one SMTP target's transport settings: collapse `tls` / the legacy
/// `starttls` into a single mode, then fill an omitted port from *that* mode.
/// Both in one place, because a port defaulted without knowing the mode is the
/// wrong-port bug #57 fixed, in the other direction.
fn smtp_transport(
    name: &str,
    tls: Option<SmtpTls>,
    starttls: Option<bool>,
    port: Option<u16>,
) -> Result<(SmtpTls, u16), NotifyError> {
    // A contradictory `tls` + `starttls` pair fails the target the same way a
    // bad address does: warned and skipped, never fatal.
    let tls = resolve_smtp_tls(tls, starttls).map_err(|reason| NotifyError::Config {
        name: name.to_string(),
        reason,
    })?;
    Ok((tls, port.unwrap_or_else(|| default_smtp_port(tls))))
}

/// Build one configured target (backend + its trigger subscription). Fallible
/// so [`Dispatcher::from_config`] can skip a bad target rather than abort.
fn build_target(
    name: &str,
    target: NotificationTarget,
    http: &reqwest::Client,
) -> Result<Target, NotifyError> {
    let (raw_triggers, notifier): (Option<Vec<String>>, Box<dyn Notifier>) = match target {
        NotificationTarget::Webhook { url, triggers } => (
            triggers,
            Box::new(WebhookNotifier::new(name, url.expose(), http.clone())),
        ),
        NotificationTarget::Discord {
            webhook_url,
            triggers,
        } => (
            triggers,
            Box::new(DiscordNotifier::new(
                name,
                webhook_url.expose(),
                http.clone(),
            )),
        ),
        NotificationTarget::Telegram {
            bot_token,
            chat_id,
            triggers,
        } => (
            triggers,
            Box::new(TelegramNotifier::new(
                name,
                bot_token,
                chat_id,
                http.clone(),
            )),
        ),
        NotificationTarget::Smtp {
            host,
            port,
            username,
            password,
            from,
            to,
            tls,
            starttls,
            triggers,
        } => {
            let (tls, port) = smtp_transport(name, tls, starttls, port)?;
            (
                triggers,
                Box::new(SmtpNotifier::new(SmtpParams {
                    name: name.to_string(),
                    host,
                    port,
                    username,
                    password,
                    from,
                    to,
                    tls,
                })?),
            )
        }
    };
    let triggers = parse_triggers(name, raw_triggers)?;
    Ok(Target { triggers, notifier })
}

/// Human phrasing for a rollback reason. Kept here (presentation) rather than on
/// the pure-data [`RollbackReason`].
fn reason_text(reason: RollbackReason) -> &'static str {
    match reason {
        RollbackReason::HealthTimeout => "health check timed out",
        RollbackReason::Crashed => "the new container crashed",
    }
}

/// The one rendered form every backend consumes. `trigger` and `container` are
/// carried as machine-readable fields for the generic webhook payload.
#[derive(Debug, Clone)]
pub struct RenderedMessage {
    pub title: String,
    pub body: String,
    pub trigger: Trigger,
    pub container: String,
}

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error("notification request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("notification target returned HTTP {0}")]
    Status(reqwest::StatusCode),
    #[error("smtp send failed: {0}")]
    Smtp(String),
    #[error("invalid notification config for `{name}`: {reason}")]
    Config { name: String, reason: String },
}

/// Shared POST-JSON path for the three HTTP backends (DRY). Strips the URL from
/// any transport error via [`reqwest::Error::without_url`] so a webhook/Discord
/// secret or a Telegram bot token embedded in the URL can never reach a log line
/// (the [`crate::config::Secret`] invariant). A non-2xx becomes a typed
/// [`NotifyError::Status`]; the dispatcher already logs which target failed.
async fn post_json<B: Serialize + ?Sized>(
    client: &reqwest::Client,
    url: &str,
    body: &B,
) -> Result<(), NotifyError> {
    let resp = client
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(NotifyError::Status(status))
    }
}

/// One notification backend. `send` takes the already-rendered message so all
/// wording stays centralized in [`NotifyEvent::render`].
#[async_trait::async_trait]
pub trait Notifier: Send + Sync {
    /// The target's config name (`[notifications.<name>]`), used only in logs.
    fn name(&self) -> &str;
    /// The backend kind (`webhook`/`discord`/`telegram`/`smtp`) for the startup
    /// summary. A `&'static str`, never a secret.
    fn kind(&self) -> &'static str;
    async fn send(&self, msg: &RenderedMessage) -> Result<(), NotifyError>;
}

/// A configured target: a backend plus the triggers it subscribes to.
struct Target {
    triggers: HashSet<Trigger>,
    notifier: Box<dyn Notifier>,
}

/// Holds every configured target. Cheap to clone (shared `Arc`) so it can be
/// passed by value into the scheduler. An empty dispatcher is a no-op.
#[derive(Clone)]
pub struct Dispatcher {
    targets: Arc<Vec<Target>>,
}

impl Dispatcher {
    /// A dispatcher with no targets — used when notifications are unconfigured
    /// and in tests that don't care about sends.
    pub fn noop() -> Self {
        Self {
            targets: Arc::new(Vec::new()),
        }
    }

    /// Build the dispatcher from parsed config, sharing one `http` client across
    /// the HTTP backends (SMTP ignores it). **Resilient**: a target that fails to
    /// build (bad trigger token, malformed SMTP relay/address) is logged and
    /// skipped, so one bad `[notifications.*]` entry can never stop the daemon
    /// from updating containers — same rule as a failed send. (Structurally
    /// broken config is already rejected earlier, when the file is parsed.)
    pub fn from_config(config: NotificationConfig, http: reqwest::Client) -> Self {
        let mut targets = Vec::with_capacity(config.targets.len());
        for (name, target) in config.targets {
            match build_target(&name, target, &http) {
                Ok(t) => targets.push(t),
                Err(e) => {
                    warn!(target = %name, error = %e, "skipping invalid notification target")
                }
            }
        }
        Self {
            targets: Arc::new(targets),
        }
    }

    #[cfg(test)]
    fn from_targets(targets: Vec<Target>) -> Self {
        Self {
            targets: Arc::new(targets),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Log a one-line startup summary of the configured targets — each as
    /// `name(kind)[triggers]` — so a typo'd or unset `FRESHDOCK_NOTIFY_*` var
    /// shows up at boot, not hours later when an update fails to notify. Mirrors
    /// the registry-credential summary. Names, kinds, and triggers only; a
    /// configured-but-empty dispatcher says so explicitly.
    pub fn log_configured(&self) {
        if self.targets.is_empty() {
            info!("no notification targets configured");
            return;
        }
        let mut summary: Vec<String> = self
            .targets
            .iter()
            .map(|t| {
                let mut triggers: Vec<&str> = t.triggers.iter().map(|x| x.as_str()).collect();
                triggers.sort_unstable();
                format!(
                    "{}({})[{}]",
                    t.notifier.name(),
                    t.notifier.kind(),
                    triggers.join(",")
                )
            })
            .collect();
        summary.sort_unstable();
        info!(count = self.targets.len(), targets = ?summary, "notification targets configured");
    }

    /// Render the event once, then send to every subscribed target. Never fails:
    /// a per-target error is logged at WARN and the next target still runs, so a
    /// flaky notifier can neither block another target nor abort the caller.
    ///
    /// Every outcome leaves a log line so a missing notification is diagnosable:
    /// a successful send logs at INFO (`notification sent`), a failure at WARN,
    /// and an event that no target subscribed to logs at DEBUG — otherwise an
    /// operator can't tell "sent and the receiver dropped it" from "nothing was
    /// subscribed to this trigger".
    pub async fn dispatch(&self, event: &NotifyEvent) {
        if self.targets.is_empty() {
            return;
        }
        let trigger = event.trigger();
        let msg = event.render();
        let mut delivered = 0usize;
        for target in self.targets.iter() {
            if !target.triggers.contains(&trigger) {
                continue;
            }
            delivered += 1;
            match target.notifier.send(&msg).await {
                Ok(()) => info!(
                    target = %target.notifier.name(),
                    trigger = %trigger.as_str(),
                    container = %msg.container,
                    "notification sent"
                ),
                Err(e) => warn!(
                    target = %target.notifier.name(),
                    error = %e,
                    "notification failed; continuing"
                ),
            }
        }
        if delivered == 0 {
            debug!(
                trigger = %trigger.as_str(),
                container = %msg.container,
                "no notification target subscribes to this trigger"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DIG: &str = "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn available() -> NotifyEvent {
        NotifyEvent::UpdateAvailable {
            container: "web".into(),
            image: "nginx:latest".into(),
            latest_digest: DIG.into(),
        }
    }
    fn succeeded() -> NotifyEvent {
        NotifyEvent::UpdateSucceeded {
            container: "web".into(),
            image: "nginx:latest".into(),
            new_id: DIG.into(),
        }
    }
    fn failed() -> NotifyEvent {
        NotifyEvent::UpdateFailed {
            container: "web".into(),
            reason: RollbackReason::HealthTimeout,
            old_image_ref: "nginx:1.0".into(),
            new_image_ref: "nginx:1.1".into(),
            restored_from: "web-old-1700000000".into(),
        }
    }

    /// Records every message it's handed; can be told to fail.
    struct RecordingNotifier {
        name: String,
        fail: bool,
        seen: Arc<Mutex<Vec<RenderedMessage>>>,
        calls: Arc<AtomicUsize>,
    }

    impl RecordingNotifier {
        fn new(name: &str) -> (Self, Arc<Mutex<Vec<RenderedMessage>>>, Arc<AtomicUsize>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    name: name.into(),
                    fail: false,
                    seen: seen.clone(),
                    calls: calls.clone(),
                },
                seen,
                calls,
            )
        }
        fn failing(name: &str) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    name: name.into(),
                    fail: true,
                    seen: Arc::new(Mutex::new(Vec::new())),
                    calls: calls.clone(),
                },
                calls,
            )
        }
    }

    #[async_trait::async_trait]
    impl Notifier for RecordingNotifier {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> &'static str {
            "recording"
        }
        async fn send(&self, msg: &RenderedMessage) -> Result<(), NotifyError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(NotifyError::Status(
                    reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                ));
            }
            self.seen.lock().unwrap().push(msg.clone());
            Ok(())
        }
    }

    fn target(triggers: HashSet<Trigger>, notifier: impl Notifier + 'static) -> Target {
        Target {
            triggers,
            notifier: Box::new(notifier),
        }
    }

    #[test]
    fn trigger_parse_roundtrips_and_rejects_junk() {
        for t in [Trigger::Available, Trigger::Succeeded, Trigger::Failed] {
            assert_eq!(Trigger::parse(t.as_str()), Ok(t));
        }
        assert_eq!(Trigger::parse("  FAILED "), Ok(Trigger::Failed));
        assert_eq!(Trigger::parse("nope"), Err("nope".to_string()));
    }

    #[test]
    fn render_maps_each_event_to_its_trigger_and_wording() {
        let a = available().render();
        assert_eq!(a.trigger, Trigger::Available);
        assert!(a.title.contains("Update available"));
        assert!(a.body.contains("watch mode"));
        // Digest is truncated via the shared helper, not printed raw.
        assert!(a.body.contains("sha256:abcdef012345…"));
        assert!(!a.body.contains(DIG));

        let s = succeeded().render();
        assert_eq!(s.trigger, Trigger::Succeeded);
        assert!(s.title.contains("Updated"));

        let f = failed().render();
        assert_eq!(f.trigger, Trigger::Failed);
        assert!(f.title.contains("Update failed"));
        assert!(f.body.contains("health check timed out"));
        assert!(f.body.contains("web-old-1700000000"));
    }

    #[tokio::test]
    async fn empty_dispatcher_is_a_noop() {
        Dispatcher::noop().dispatch(&succeeded()).await; // must not panic
        assert!(Dispatcher::noop().is_empty());
    }

    #[tokio::test]
    async fn only_subscribed_targets_receive_an_event() {
        let (failures_only, seen_f, calls_f) = RecordingNotifier::new("failures");
        let (all, seen_a, calls_a) = RecordingNotifier::new("all");
        let d = Dispatcher::from_targets(vec![
            target([Trigger::Failed].into_iter().collect(), failures_only),
            target(Trigger::all(), all),
        ]);

        d.dispatch(&succeeded()).await;
        assert_eq!(
            calls_f.load(Ordering::SeqCst),
            0,
            "failures-only skips success"
        );
        assert_eq!(
            calls_a.load(Ordering::SeqCst),
            1,
            "all-subscriber gets success"
        );
        assert!(seen_f.lock().unwrap().is_empty());
        assert_eq!(seen_a.lock().unwrap().len(), 1);

        d.dispatch(&failed()).await;
        assert_eq!(
            calls_f.load(Ordering::SeqCst),
            1,
            "failures-only gets failure"
        );
        assert_eq!(
            calls_a.load(Ordering::SeqCst),
            2,
            "all-subscriber also gets failure"
        );
    }

    #[tokio::test]
    async fn a_failing_target_does_not_block_a_later_one() {
        let (boom, boom_calls) = RecordingNotifier::failing("boom");
        let (ok, seen_ok, ok_calls) = RecordingNotifier::new("ok");
        let d = Dispatcher::from_targets(vec![
            target(Trigger::all(), boom),
            target(Trigger::all(), ok),
        ]);

        d.dispatch(&succeeded()).await; // boom errors; ok must still receive
        assert_eq!(boom_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ok_calls.load(Ordering::SeqCst), 1);
        assert_eq!(seen_ok.lock().unwrap().len(), 1);
    }

    #[test]
    fn omitted_triggers_subscribe_to_all_three() {
        assert_eq!(parse_triggers("x", None).unwrap(), Trigger::all());
    }

    #[test]
    fn empty_triggers_list_subscribes_to_nothing() {
        // `triggers = []` is a valid "disable this target" — distinct from
        // omitting the key (which subscribes to all).
        assert!(parse_triggers("x", Some(vec![])).unwrap().is_empty());
    }

    // --- smtp transport resolution: tls mode + the port that goes with it ---

    #[test]
    fn default_smtp_port_follows_the_tls_mode() {
        assert_eq!(default_smtp_port(SmtpTls::Starttls), 587);
        assert_eq!(default_smtp_port(SmtpTls::Implicit), 465);
        assert_eq!(default_smtp_port(SmtpTls::Plaintext), 25);
    }

    #[test]
    fn smtp_transport_defaults_the_port_from_the_resolved_mode() {
        // Implicit TLS on 587 can never complete a handshake — the #57
        // wrong-port bug in the other direction, so the default must follow the
        // mode rather than being a fixed 587.
        assert_eq!(
            smtp_transport("mail", Some(SmtpTls::Implicit), None, None).unwrap(),
            (SmtpTls::Implicit, 465)
        );
        // …including when the mode came from the legacy key.
        assert_eq!(
            smtp_transport("mail", None, Some(false), None).unwrap(),
            (SmtpTls::Implicit, 465)
        );
        assert_eq!(
            smtp_transport("mail", None, None, None).unwrap(),
            (SmtpTls::Starttls, 587)
        );
        assert_eq!(
            smtp_transport("mail", Some(SmtpTls::Plaintext), None, None).unwrap(),
            (SmtpTls::Plaintext, 25)
        );
        // An explicit port always wins over the mode default.
        assert_eq!(
            smtp_transport("mail", Some(SmtpTls::Plaintext), None, Some(1025)).unwrap(),
            (SmtpTls::Plaintext, 1025)
        );
    }

    /// A minimal file-declared smtp target, so a test can vary only the tls
    /// pair and the port.
    fn smtp_target(
        port: Option<u16>,
        tls: Option<SmtpTls>,
        starttls: Option<bool>,
    ) -> NotificationTarget {
        NotificationTarget::Smtp {
            host: "smtp.example.com".to_string(),
            port,
            username: None,
            password: None,
            from: "freshdock@example.com".to_string(),
            to: vec!["admin@example.com".to_string()],
            tls,
            starttls,
            triggers: None,
        }
    }

    #[test]
    fn build_target_accepts_an_agreeing_tls_pair_and_a_missing_port() {
        // `tls = "implicit"` alongside a not-yet-removed `starttls = false`, and
        // no port: both resolutions happen here, and the target survives.
        let built = build_target(
            "mail",
            smtp_target(None, Some(SmtpTls::Implicit), Some(false)),
            &crate::http::client(),
        );
        assert!(built.is_ok(), "an agreeing pair must not drop the target");
    }

    #[test]
    fn build_target_rejects_a_contradictory_tls_pair() {
        // `match` rather than `unwrap_err`: `Target` wraps a non-Debug notifier.
        let err = match build_target(
            "mail",
            smtp_target(None, Some(SmtpTls::Plaintext), Some(true)),
            &crate::http::client(),
        ) {
            Err(e) => e,
            Ok(_) => panic!("a contradictory tls pair must be rejected"),
        };
        assert!(matches!(err, NotifyError::Config { .. }), "{err}");
        assert!(err.to_string().contains("mail"), "{err}");
    }

    #[test]
    fn from_config_skips_a_target_with_an_unknown_trigger() {
        use crate::config::{NotificationConfig, NotificationTarget, Secret};
        let mut targets = std::collections::HashMap::new();
        targets.insert(
            "hook".to_string(),
            NotificationTarget::Webhook {
                url: Secret::new("https://example.com"),
                triggers: Some(vec!["bogus".to_string()]),
            },
        );
        // Resilient: the bad target is dropped, not an error — the daemon keeps
        // running (here with no targets left).
        let d = Dispatcher::from_config(NotificationConfig { targets }, crate::http::client());
        assert!(d.is_empty());
    }

    /// A second dispatcher, registered once and never dropped, that discards
    /// everything. It exists only to keep tracing-core off its
    /// single-dispatcher fast path: with just one live dispatcher, callsite
    /// interest is rebuilt from whatever subscriber the *rebuilding thread*
    /// happens to have, so a test running in parallel — holding no subscriber —
    /// can cache a callsite as "never" and blank a capture below. With two
    /// registered dispatchers, interest falls back to the per-event `enabled`
    /// path, which asks *this* thread's subscriber. Without this, the log
    /// assertions here fail intermittently under `cargo test`'s parallelism.
    static KEEPALIVE: std::sync::LazyLock<tracing::Dispatch> = std::sync::LazyLock::new(|| {
        tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(std::io::sink)
                .with_ansi(false)
                .finish(),
        )
    });

    /// Run `f` under a temporary tracing subscriber that captures into a string,
    /// so a test can assert on real log output.
    fn capture_logs(f: impl FnOnce()) -> String {
        use std::io::Write;
        use tracing_subscriber::fmt::MakeWriter;

        std::sync::LazyLock::force(&KEEPALIVE);

        #[derive(Clone)]
        struct BufWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for BufWriter {
            type Writer = BufWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(BufWriter(buf.clone()))
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn a_successful_send_logs_notification_sent() {
        let (ok, _seen, _calls) = RecordingNotifier::new("ops");
        let d = Dispatcher::from_targets(vec![target(Trigger::all(), ok)]);
        let out = capture_logs(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(d.dispatch(&succeeded()));
        });
        assert!(
            out.contains("notification sent"),
            "expected the send log: {out}"
        );
        assert!(out.contains("ops"), "target name present: {out}");
        assert!(out.contains("succeeded"), "trigger present: {out}");
    }

    #[test]
    fn log_configured_summarizes_targets_without_leaking_secrets() {
        use crate::config::{NotificationConfig, NotificationTarget, Secret};
        let mut targets = std::collections::HashMap::new();
        targets.insert(
            "ops".to_string(),
            NotificationTarget::Webhook {
                url: Secret::new("https://example.com/hook"),
                triggers: Some(vec!["succeeded".to_string()]),
            },
        );
        targets.insert(
            "chat".to_string(),
            NotificationTarget::Discord {
                webhook_url: Secret::new("https://discord.com/api/webhooks/1/SECRETTOKEN"),
                triggers: None,
            },
        );
        let d = Dispatcher::from_config(NotificationConfig { targets }, crate::http::client());
        let out = capture_logs(|| d.log_configured());
        assert!(out.contains("notification targets configured"), "{out}");
        assert!(out.contains("ops(webhook)"), "{out}");
        assert!(out.contains("chat(discord)"), "{out}");
        assert!(out.contains("succeeded"), "{out}");
        assert!(
            !out.contains("SECRETTOKEN"),
            "the summary must never print a secret: {out}"
        );
    }

    #[test]
    fn log_configured_announces_when_empty() {
        let out = capture_logs(|| Dispatcher::noop().log_configured());
        assert!(out.contains("no notification targets configured"), "{out}");
    }
}
