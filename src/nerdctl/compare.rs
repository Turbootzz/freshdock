//! Did the rebuilt flags reproduce the container?
//!
//! Preflight creates a trial container from the rebuilt flags and the *same*
//! image the original runs. Their OCI runtime specs and labels should then
//! match, apart from what is naturally per-container (id, name, state paths).
//! Anything else that differs is a setting nerdctl's inspect never reported,
//! so freshdock could not carry it over, and the update is refused.

use serde_json::{Map, Value};

/// Annotations and labels that are per-container bookkeeping, or that the
/// trial deliberately lacks (published ports, which it is created without).
const PER_CONTAINER_KEYS: [&str; 12] = [
    "nerdctl/name",
    "nerdctl/state-dir",
    "nerdctl/log-uri",
    "nerdctl/hostname",
    "nerdctl/ports",
    "nerdctl/anonymous-volumes",
    "nerdctl/healthstate",
    "nerdctl/error",
    "containerd.io/restart.count",
    "containerd.io/restart.status",
    "containerd.io/restart.explicitly-stopped",
    "containerd.io/restart.loguri",
];

/// How many differing paths to report; the first few say enough.
const MAX_REPORTED: usize = 6;

/// The paths at which two containers' settings differ, given each one's
/// `nerdctl inspect --mode=native` element and id. Empty when the second
/// reproduces the first.
pub fn differences(
    original: &Value,
    original_id: &str,
    trial: &Value,
    trial_id: &str,
) -> Vec<String> {
    let a = normalize(original, original_id);
    let b = normalize(trial, trial_id);
    let mut out = Vec::new();
    diff("", &a, &b, &mut out);
    out
}

/// The settings part of a native inspect, with per-container values replaced
/// by placeholders.
fn normalize(native: &Value, id: &str) -> Value {
    let short = id.get(..12).unwrap_or(id);
    let mut spec = native.get("Spec").cloned().unwrap_or(Value::Null);
    if let Some(s) = spec.as_object_mut() {
        s.remove("root");
        // nerdctl hands its hooks the environment of whichever process ran
        // the create (a shell, or freshdock): not a container setting.
        if let Some(Value::Object(hooks)) = s.get_mut("hooks") {
            for entry in hooks
                .values_mut()
                .filter_map(Value::as_array_mut)
                .flatten()
                .filter_map(Value::as_object_mut)
            {
                entry.remove("env");
            }
        }
        if s.get("hostname").and_then(Value::as_str) == Some(short) {
            s.insert("hostname".to_owned(), Value::from("SHORT-ID"));
        }
        if let Some(Value::Array(env)) = s.get_mut("process").and_then(|p| p.get_mut("env")) {
            env.retain(|v| !v.as_str().is_some_and(|e| e.starts_with("HOSTNAME=")));
        }
        if let Some(Value::Object(annotations)) = s.get_mut("annotations") {
            drop_per_container(annotations);
        }
        if let Some(Value::Object(linux)) = s.get_mut("linux") {
            linux.remove("cgroupsPath");
            if let Some(Value::Array(namespaces)) = linux.get_mut("namespaces") {
                for ns in namespaces.iter_mut().filter_map(Value::as_object_mut) {
                    ns.remove("path");
                }
            }
        }
        // Mount sources in nerdctl's state (resolv.conf, hosts, hostname) are
        // per-container paths, but they differ only by the container id, which
        // `replace_ids` below turns into a placeholder whatever the data root.
    }
    let mut labels = native.get("Labels").cloned().unwrap_or(Value::Null);
    if let Value::Object(l) = &mut labels {
        drop_per_container(l);
    }
    let mut both = Map::new();
    both.insert("Spec".to_owned(), spec);
    both.insert("Labels".to_owned(), labels);
    replace_ids(Value::Object(both), id, short)
}

fn drop_per_container(map: &mut Map<String, Value>) {
    for key in PER_CONTAINER_KEYS {
        map.remove(key);
    }
}

fn replace_ids(v: Value, id: &str, short: &str) -> Value {
    match v {
        Value::String(s) if !id.is_empty() => {
            Value::String(s.replace(id, "ID").replace(short, "SHORT-ID"))
        }
        Value::Array(a) => Value::Array(a.into_iter().map(|v| replace_ids(v, id, short)).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| (k, replace_ids(v, id, short)))
                .collect(),
        ),
        other => other,
    }
}

fn diff(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
    if out.len() >= MAX_REPORTED || a == b {
        return;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let child = format!("{path}.{k}");
                match (x.get(k), y.get(k)) {
                    (Some(va), Some(vb)) => diff(&child, va, vb, out),
                    (Some(_), None) => push(out, format!("{child} is missing")),
                    (None, Some(_)) => push(out, format!("{child} would be added")),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (va, vb)) in x.iter().zip(y).enumerate() {
                diff(&format!("{path}[{i}]"), va, vb, out);
            }
        }
        _ => {
            // Environment values are often secrets, and this message reaches
            // logs and notifications: name the variables, never their values.
            let env = path.trim_start_matches('.').starts_with("Spec.process.env");
            let show = |v: &Value| if env { redacted(v) } else { brief(v) };
            push(
                out,
                format!("{path} would change from {} to {}", show(a), show(b)),
            )
        }
    }
}

/// `KEY=VALUE` strings (alone or in an array) with the values replaced.
fn redacted(v: &Value) -> String {
    let key = |s: &str| format!("{}=<redacted>", s.split('=').next().unwrap_or(""));
    match v {
        Value::String(s) => format!("\"{}\"", key(s)),
        Value::Array(items) => brief(&Value::Array(
            items
                .iter()
                .map(|i| Value::String(i.as_str().map_or_else(|| "<redacted>".to_owned(), key)))
                .collect(),
        )),
        _ => "<redacted>".to_owned(),
    }
}

fn push(out: &mut Vec<String>, s: String) {
    if out.len() < MAX_REPORTED {
        out.push(s.trim_start_matches('.').to_owned());
    }
}

fn brief(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 60 {
        format!("{}…", s.chars().take(60).collect::<String>())
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ID_A: &str = "aaaaaaaaaaaa1111111111111111111111111111111111111111111111111111";
    const ID_B: &str = "bbbbbbbbbbbb2222222222222222222222222222222222222222222222222222";

    fn native(id: &str, extra: Value) -> Value {
        let short = &id[..12];
        let mut v = json!({
            "ID": id,
            "Labels": {
                "nerdctl/name": format!("c-{short}"),
                "nerdctl/state-dir": format!("/var/lib/nerdctl/x/containers/ns/{id}"),
                "freshdock.enable": "true"
            },
            "Spec": {
                "hostname": short,
                "root": {"path": "rootfs"},
                "process": {"env": ["PATH=/bin", format!("HOSTNAME={short}")], "cwd": "/"},
                "annotations": {"nerdctl/name": format!("c-{short}"), "nerdctl/platform": "linux/amd64"},
                "linux": {
                    "cgroupsPath": format!("system.slice:nerdctl:{id}"),
                    "namespaces": [{"type": "pid"}, {"type": "network", "path": format!("/proc/{short}/ns/net")}]
                },
                "mounts": [{"destination": "/etc/resolv.conf", "source": format!("/var/lib/nerdctl/x/containers/ns/{id}/resolv.conf")}]
            }
        });
        merge(&mut v, extra);
        v
    }

    fn merge(into: &mut Value, extra: Value) {
        match (into, extra) {
            (Value::Object(a), Value::Object(b)) => {
                for (k, v) in b {
                    merge(a.entry(k).or_insert(Value::Null), v);
                }
            }
            (slot, v) => *slot = v,
        }
    }

    #[test]
    fn hook_environments_differ_by_creator_and_are_ignored() {
        let hook = |env: &str, arg: &str| {
            json!({"Spec": {"hooks": {"createRuntime": [
                {"path": "/usr/local/bin/nerdctl", "args": ["nerdctl", arg], "env": [env]}
            ]}}})
        };
        let original = native(ID_A, hook("PATH=/usr/bin", "oci-hook"));
        let trial = native(ID_B, hook("PATH=/root/bin:/usr/bin", "oci-hook"));
        assert!(differences(&original, ID_A, &trial, ID_B).is_empty());
        let other_hook = native(ID_B, hook("PATH=/usr/bin", "something-else"));
        assert_eq!(
            differences(&original, ID_A, &other_hook, ID_B),
            [
                "Spec.hooks.createRuntime[0].args[1] would change from \"oci-hook\" to \"something-else\""
            ],
            "the hook itself still has to match"
        );
    }

    #[test]
    fn two_containers_with_the_same_settings_match() {
        assert_eq!(
            differences(
                &native(ID_A, json!({})),
                ID_A,
                &native(ID_B, json!({})),
                ID_B
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_lost_security_setting_is_reported_by_path() {
        let original = native(
            ID_A,
            json!({"Spec": {"process": {"noNewPrivileges": true}}}),
        );
        let trial = native(ID_B, json!({}));
        assert_eq!(
            differences(&original, ID_A, &trial, ID_B),
            ["Spec.process.noNewPrivileges is missing"]
        );
    }

    #[test]
    fn a_changed_namespace_set_is_reported() {
        let original = native(
            ID_A,
            json!({"Spec": {"linux": {"namespaces": [{"type": "network"}]}}}),
        );
        let trial = native(ID_B, json!({}));
        let d = differences(&original, ID_A, &trial, ID_B);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(
            d[0].starts_with("Spec.linux.namespaces would change"),
            "{d:?}"
        );
    }

    #[test]
    fn a_label_the_trial_lacks_is_reported_but_bookkeeping_is_not() {
        let original = native(
            ID_A,
            json!({"Labels": {"nerdctl/domainname": "example.test", "containerd.io/restart.count": "3"}}),
        );
        let trial = native(ID_B, json!({}));
        assert_eq!(
            differences(&original, ID_A, &trial, ID_B),
            ["Labels.nerdctl/domainname is missing"]
        );
    }

    #[test]
    fn environment_values_never_appear_in_the_differences() {
        let env = |vars: &[&str]| json!({"Spec": {"process": {"env": vars}}});
        let original = native(ID_A, env(&["PATH=/bin", "API_KEY=sk-live-secret"]));
        let changed = native(ID_B, env(&["PATH=/bin", "API_KEY=sk-other-secret"]));
        let shorter = native(ID_B, env(&["PATH=/bin"]));
        for trial in [&changed, &shorter] {
            let d = differences(&original, ID_A, trial, ID_B).join("; ");
            assert!(d.contains("API_KEY=<redacted>"), "{d}");
            assert!(!d.contains("sk-"), "{d}");
        }
    }

    #[test]
    fn state_paths_match_under_any_data_root() {
        let mounts = |id: &str| {
            json!({"Spec": {"mounts": [
                {"destination": "/etc/resolv.conf", "source": format!("/srv/nerdctl-data/abc/containers/ns/{id}/resolv.conf")}
            ]}})
        };
        assert!(
            differences(
                &native(ID_A, mounts(ID_A)),
                ID_A,
                &native(ID_B, mounts(ID_B)),
                ID_B
            )
            .is_empty()
        );
    }

    #[test]
    fn an_explicit_hostname_must_match() {
        let original = native(ID_A, json!({"Spec": {"hostname": "web"}}));
        let trial = native(ID_B, json!({"Spec": {"hostname": "web"}}));
        assert!(differences(&original, ID_A, &trial, ID_B).is_empty());
        let trial = native(ID_B, json!({}));
        assert_eq!(
            differences(&original, ID_A, &trial, ID_B),
            ["Spec.hostname would change from \"web\" to \"SHORT-ID\""]
        );
    }
}
