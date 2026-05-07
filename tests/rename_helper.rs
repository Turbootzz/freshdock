use freshdock::docker::rename::{next_available_old_name, old_name_for};

#[test]
fn old_name_for_appends_unix_timestamp() {
    assert_eq!(old_name_for("nginx", 1_700_000_000), "nginx-old-1700000000");
}

#[test]
fn old_name_for_preserves_dashes_in_original_name() {
    assert_eq!(old_name_for("svc-with-dash", 0), "svc-with-dash-old-0");
}

#[test]
fn old_name_for_handles_negative_timestamps() {
    // Pre-1970 is nonsense in practice but the helper should not panic — it
    // is a pure formatting utility.
    assert_eq!(old_name_for("box", -1), "box-old--1");
}

#[test]
fn next_available_old_name_returns_base_when_unused() {
    let result = next_available_old_name("nginx", 42, |_| false);
    assert_eq!(result, "nginx-old-42");
}

#[test]
fn next_available_old_name_appends_suffix_on_collision() {
    let taken = ["nginx-old-42".to_owned(), "nginx-old-42-1".to_owned()];
    let result = next_available_old_name("nginx", 42, |candidate| {
        taken.iter().any(|t| t == candidate)
    });
    assert_eq!(result, "nginx-old-42-2");
}

#[test]
fn next_available_old_name_walks_until_free_slot() {
    // First three suffixes are taken — helper must keep walking.
    let taken: Vec<String> = (0..=2)
        .map(|i| {
            if i == 0 {
                "svc-old-7".to_owned()
            } else {
                format!("svc-old-7-{i}")
            }
        })
        .collect();
    let result = next_available_old_name("svc", 7, |c| taken.iter().any(|t| t == c));
    assert_eq!(result, "svc-old-7-3");
}
