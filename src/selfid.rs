//! Recognise freshdock's own container (issue #79), shared by the `watch_all`
//! sweep guard and the network-dependent re-attach pass so "which container is
//! me" has a single answer.
//!
//! Docker defaults a container's hostname to the 12-hex short container id, so
//! a container-id-shaped hostname identifies us. A custom hostname (or running
//! outside a container) yields `None` and no container is treated as ours.
//! Known limit: under `network_mode: container:<x>` the hostname is the
//! namespace owner's, not ours.

/// Our own container id prefix, taken from the hostname, or `None` when the
/// hostname is not container-id-shaped. `/etc/hostname` is the reading that
/// survives a `docker exec` environment, with `$HOSTNAME` as the fallback.
pub fn own_container_id_prefix() -> Option<String> {
    let from_file = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|h| !h.is_empty());
    from_file
        .or_else(|| std::env::var("HOSTNAME").ok())
        .filter(|h| looks_like_container_id(h))
        .map(|h| h.to_ascii_lowercase())
}

/// Could `name` be a container id (or short id)? Docker's short id is 12 hex
/// characters; anything shorter or non-hex is an operator-chosen hostname and
/// must never be prefix-matched against container ids.
fn looks_like_container_id(name: &str) -> bool {
    name.len() >= 12 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Does `id` belong to the container `prefix` came from? Validates the prefix
/// shape itself so a raw hostname can be passed safely.
pub fn is_own_container(prefix: Option<&str>, id: Option<&str>) -> bool {
    let (Some(p), Some(i)) = (prefix, id) else {
        return false;
    };
    looks_like_container_id(p) && i.starts_with(&p.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "abc123def4567890abc123def4567890abc123def4567890abc123def4567890";

    #[test]
    fn is_own_container_matches_only_the_container_the_prefix_came_from() {
        assert!(is_own_container(Some("abc123def456"), Some(ID)));
        assert!(!is_own_container(Some("ffffffffffff"), Some(ID)));
        assert!(!is_own_container(None, Some(ID)));
        assert!(!is_own_container(Some("abc123def456"), None));
        assert!(!is_own_container(None, None));
        assert!(
            !is_own_container(Some(""), Some(ID)),
            "an empty prefix must never match every container"
        );
        assert!(
            !is_own_container(Some("my-web-server"), Some(ID)),
            "an operator-chosen hostname is never prefix-matched"
        );
    }

    #[test]
    fn full_and_uppercase_ids_match() {
        assert!(is_own_container(Some(ID), Some(ID)), "a 64-hex hostname");
        assert!(
            is_own_container(Some("ABC123DEF456"), Some(ID)),
            "ids are lowercase; an uppercase-hex hostname still matches"
        );
    }

    #[test]
    fn looks_like_container_id_requires_at_least_twelve_hex_chars() {
        assert!(looks_like_container_id("abc123def456"));
        assert!(looks_like_container_id(ID), "a full 64-hex id counts");
        assert!(!looks_like_container_id("abc123def45"), "too short");
        assert!(!looks_like_container_id("my-web-server"), "not hex");
        assert!(!looks_like_container_id(""), "empty");
    }
}
