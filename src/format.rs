//! Small display helpers shared across the status table and notification
//! rendering, so digest/id truncation reads the same everywhere (DRY).

/// Truncate a digest or ref for human display: a `sha256:` digest keeps its
/// first 12 hex chars, any other long string is cut at 19 chars, and short
/// values pass through unchanged. The `…` marks that truncation happened.
pub fn short_digest(d: &str) -> String {
    if let Some(hex) = d.strip_prefix("sha256:") {
        format!("sha256:{}…", &hex[..hex.len().min(12)])
    } else if d.len() > 19 {
        format!("{}…", &d[..19])
    } else {
        d.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_digest_truncates_sha256() {
        let d = "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert_eq!(short_digest(d), "sha256:abcdef012345…");
    }

    #[test]
    fn short_digest_passes_through_non_sha() {
        assert_eq!(short_digest("alpine:latest"), "alpine:latest");
    }
}
