//! The one place the freshdock outbound HTTP client is configured, so the OCI
//! registry digest flow and the Phase 6 HTTP notification backends share one
//! TLS stack, timeout, and user-agent instead of each building their own (DRY).
//!
//! `reqwest::Client` is `Clone` and pools connections internally, so callers
//! clone the single instance rather than constructing a second one.

use std::time::Duration;

/// Outbound request timeout. Generous enough for a slow registry HEAD or a
/// webhook round-trip, short enough that a hung peer can't stall a daemon tick.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the shared client: rustls (via the `reqwest` Cargo features), a 30s
/// timeout, and a `freshdock/{version}` user-agent.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("freshdock/", env!("CARGO_PKG_VERSION")))
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("reqwest client construction with default config cannot fail")
}
