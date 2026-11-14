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

/// The shared HTTP client could not be built.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The TLS root store is empty: no CA bundle, or `SSL_CERT_*` points nowhere.
    #[error(
        "no CA certificates were found ({cause}): freshdock needs a CA bundle for registry \
         and notification HTTPS. Install ca-certificates in the image or mount a bundle at \
         /etc/ssl/certs/ca-certificates.crt; when SSL_CERT_FILE or SSL_CERT_DIR is set only \
         that location is read"
    )]
    NoCaStore { cause: String },
    /// Any other client-builder failure.
    #[error("could not build the HTTP client: {0}")]
    Build(#[source] reqwest::Error),
}

/// Walks the source chain; reqwest's own `Display` is only `builder error`.
fn empty_ca_store_cause(error: &dyn std::error::Error) -> Option<String> {
    std::iter::successors(Some(error), |e| e.source())
        .map(ToString::to_string)
        .find(|message| message.contains("No CA certificates"))
}

/// Build the shared client: rustls (via the `reqwest` Cargo features), a 30s
/// timeout, and a `freshdock/{version}` user-agent.
pub fn client() -> Result<reqwest::Client, HttpError> {
    reqwest::Client::builder()
        .user_agent(concat!("freshdock/", env!("CARGO_PKG_VERSION")))
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| match empty_ca_store_cause(&e) {
            Some(cause) => HttpError::NoCaStore { cause },
            None => HttpError::Build(e),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("No CA certificates were loaded from the system")]
    struct EmptyStore;

    #[derive(Debug, thiserror::Error)]
    #[error("connection refused")]
    struct Refused;

    #[derive(Debug, thiserror::Error)]
    #[error("builder error")]
    struct Builder<E: std::error::Error + 'static>(#[source] E);

    #[test]
    fn the_ca_cause_is_found_down_the_source_chain() {
        assert_eq!(
            empty_ca_store_cause(&Builder(EmptyStore)),
            Some("No CA certificates were loaded from the system".to_owned())
        );
    }

    #[test]
    fn another_chain_is_not_mistaken_for_a_ca_failure() {
        assert_eq!(empty_ca_store_cause(&Builder(Refused)), None);
    }
}
