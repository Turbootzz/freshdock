use bollard::models::ContainerInspectResponse;
use freshdock::docker::spec::ContainerSpec;

const FIXTURE: &str = include_str!("fixtures/container_inspect.json");
const FIXTURE_WEIRD: &str = include_str!("fixtures/container_inspect_weird.json");
const NEW_IMAGE: &str =
    "nginx@sha256:2222222222222222222222222222222222222222222222222222222222222222";

fn load_inspect() -> ContainerInspectResponse {
    serde_json::from_str(FIXTURE).expect("fixture should deserialize into ContainerInspectResponse")
}

fn load_spec() -> ContainerSpec {
    ContainerSpec::from_inspect(load_inspect()).expect("spec should build from fixture")
}

fn load_weird_spec() -> ContainerSpec {
    let inspect: ContainerInspectResponse = serde_json::from_str(FIXTURE_WEIRD)
        .expect("weird fixture should deserialize into ContainerInspectResponse");
    ContainerSpec::from_inspect(inspect).expect("weird spec should build from fixture")
}

#[test]
fn captures_identity_and_image_from_inspect() {
    let spec = load_spec();

    assert_eq!(
        spec.name, "fd-smoke",
        "leading slash from Docker name should be stripped"
    );
    assert_eq!(
        spec.image_ref, "nginx:alpine",
        "image_ref should come from Config.Image (the original ref), not the resolved digest in Image"
    );
}

#[test]
fn create_body_carries_env_cmd_entrypoint_and_image_override() {
    let spec = load_spec();
    let body = spec.to_create_body(NEW_IMAGE);

    assert_eq!(
        body.image.as_deref(),
        Some(NEW_IMAGE),
        "create body must use the recreate digest, not the original tag"
    );
    assert_eq!(
        body.env.as_deref(),
        Some(
            &[
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned(),
                "NGINX_VERSION=1.27.0".to_owned(),
                "FD_TEST=hello".to_owned(),
            ][..]
        )
    );
    assert_eq!(
        body.cmd.as_deref(),
        Some(
            &[
                "nginx".to_owned(),
                "-g".to_owned(),
                "daemon off;".to_owned()
            ][..]
        )
    );
    assert_eq!(
        body.entrypoint.as_deref(),
        Some(&["/docker-entrypoint.sh".to_owned()][..])
    );
    assert_eq!(body.working_dir.as_deref(), Some("/usr/share/nginx/html"));
    assert_eq!(body.user.as_deref(), Some("root"));
    assert_eq!(body.stop_signal.as_deref(), Some("SIGTERM"));
    assert_eq!(body.stop_timeout, Some(30));
}

#[test]
fn create_body_preserves_healthcheck_intervals_in_nanoseconds() {
    let spec = load_spec();
    let body = spec.to_create_body(NEW_IMAGE);
    let hc = body.healthcheck.expect("healthcheck must round-trip");

    assert_eq!(
        hc.test.as_deref(),
        Some(
            &[
                "CMD-SHELL".to_owned(),
                "curl -f http://localhost/ || exit 1".to_owned(),
            ][..]
        )
    );
    assert_eq!(hc.interval, Some(30_000_000_000));
    assert_eq!(hc.timeout, Some(5_000_000_000));
    assert_eq!(hc.retries, Some(3));
    assert_eq!(hc.start_period, Some(10_000_000_000));
}

#[test]
fn create_body_preserves_host_config_essentials() {
    let spec = load_spec();
    let body = spec.to_create_body(NEW_IMAGE);
    let host = body.host_config.expect("host_config must round-trip");

    let binds = host.binds.expect("binds must round-trip");
    assert!(binds.contains(&"/host/data:/data:rw".to_owned()));
    assert!(binds.contains(&"/host/conf:/etc/nginx/conf.d:ro".to_owned()));

    let policy = host.restart_policy.expect("restart_policy must round-trip");
    assert_eq!(
        policy.name,
        Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED)
    );

    assert_eq!(host.cap_add.as_deref(), Some(&["NET_ADMIN".to_owned()][..]));
    assert_eq!(host.cap_drop.as_deref(), Some(&["MKNOD".to_owned()][..]));

    let sysctls = host.sysctls.expect("sysctls must round-trip");
    assert_eq!(
        sysctls.get("net.ipv4.ip_forward").map(String::as_str),
        Some("1")
    );

    let log = host.log_config.expect("log_config must round-trip");
    assert_eq!(log.typ.as_deref(), Some("json-file"));
    assert_eq!(
        log.config
            .as_ref()
            .and_then(|c| c.get("max-size"))
            .map(String::as_str),
        Some("10m")
    );
}

#[test]
fn create_body_carries_network_endpoint_with_alias_and_static_ip() {
    let spec = load_spec();
    let body = spec.to_create_body(NEW_IMAGE);
    let net_cfg = body
        .networking_config
        .expect("networking_config must round-trip");
    let endpoints = net_cfg
        .endpoints_config
        .expect("endpoints_config must round-trip");

    let endpoint = endpoints
        .get("fd-net")
        .expect("fd-net endpoint must round-trip");
    let aliases = endpoint
        .aliases
        .as_ref()
        .expect("aliases must round-trip from NetworkSettings.Networks.<name>.Aliases");
    assert!(aliases.iter().any(|a| a == "nginx-alias"));

    let ipam = endpoint
        .ipam_config
        .as_ref()
        .expect("ipam_config must round-trip for the static IP");
    assert_eq!(ipam.ipv4_address.as_deref(), Some("172.30.0.42"));
}

#[test]
fn create_options_carry_container_name() {
    let spec = load_spec();
    let opts = spec.to_create_options();
    assert_eq!(opts.name.as_deref(), Some("fd-smoke"));
}

/// Acceptance criterion #3 of issue #8: fields the user did not set in the
/// original create are not "fabricated" with surprising defaults — preserve
/// `None`. Without this guard, a recreate could quietly add a healthcheck
/// the user never wrote, or a restart policy they never opted into.
#[test]
fn unset_fields_in_inspect_stay_none_in_spec_and_create_body() {
    let minimal = r#"{
        "Name": "/bare",
        "Config": {
            "Image": "alpine:3.19",
            "Cmd": ["sh"]
        }
    }"#;

    let inspect: ContainerInspectResponse =
        serde_json::from_str(minimal).expect("minimal fixture should deserialize");
    let spec = ContainerSpec::from_inspect(inspect).expect("minimal spec should build");

    assert!(
        spec.config.healthcheck.is_none(),
        "no healthcheck in source must not synthesize one in spec"
    );
    assert!(spec.host_config.is_none(), "no HostConfig must stay None");
    assert!(
        spec.network_endpoints.is_none(),
        "no NetworkSettings.Networks must stay None"
    );
    assert!(
        spec.config.env.is_none(),
        "no env must stay None, not Some(vec![])"
    );
    assert!(
        spec.config.entrypoint.is_none(),
        "no entrypoint must stay None"
    );
    assert!(
        spec.config.stop_signal.is_none(),
        "no stop_signal must stay None"
    );

    let body = spec.to_create_body(NEW_IMAGE);
    assert!(body.healthcheck.is_none());
    assert!(body.host_config.is_none());
    assert!(body.networking_config.is_none());
    assert!(body.env.is_none());
    assert!(body.entrypoint.is_none());
}

// ---------------------------------------------------------------------------
// Weird-fixture round-trip pins (issue #25, item 3)
//
// One #[test] per HostConfig/Config dimension that the basic fixture doesn't
// already cover (or covers with a different value). Each one is mechanical —
// "weird-fixture value in, same value out of `to_create_body`" — but every
// dimension pinned here is one less thing to verify on a real daemon next
// phase. The fixture also uses a non-Hub image (`ghcr.io/owner/repo:v1`) so
// the suite pins that `ImageRef::parse` round-trip works for refs that don't
// trip the `library/` prefix.
// ---------------------------------------------------------------------------

#[test]
fn weird_spec_captures_non_hub_image_ref_unchanged() {
    let spec = load_weird_spec();
    assert_eq!(
        spec.name, "fd-smoke-weird",
        "leading slash from Docker name should be stripped"
    );
    assert_eq!(
        spec.image_ref, "ghcr.io/owner/repo:v1",
        "non-Hub refs (which ImageRef::parse passes through) must round-trip \
         from Config.Image byte-identical — companion to the #25 Hub test"
    );
}

#[test]
fn weird_spec_preserves_user_and_env() {
    let body = load_weird_spec().to_create_body(NEW_IMAGE);

    assert_eq!(
        body.user.as_deref(),
        Some("1000:1000"),
        "uid:gid form must round-trip exactly, not collapse to just `1000`"
    );
    assert_eq!(
        body.env.as_deref(),
        Some(
            &[
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned(),
                "APP_MODE=production".to_owned(),
                "APP_TOKEN=base64=padded==".to_owned(),
                "EMPTY_VAR=".to_owned(),
            ][..]
        ),
        "env entries with extra `=` and empty values must round-trip verbatim"
    );
}

#[test]
fn weird_spec_preserves_binds_and_tmpfs() {
    let host = load_weird_spec()
        .to_create_body(NEW_IMAGE)
        .host_config
        .expect("host_config must round-trip");

    let binds = host.binds.expect("binds must round-trip");
    assert!(binds.contains(&"/host/state:/var/lib/state:rw".to_owned()));
    assert!(binds.contains(&"/host/secrets:/run/secrets:ro".to_owned()));

    let tmpfs = host
        .tmpfs
        .expect("HostConfig.Tmpfs dict must round-trip (separate from Mounts)");
    assert_eq!(tmpfs.get("/run").map(String::as_str), Some("rw,size=32m"));
    assert_eq!(
        tmpfs.get("/var/cache").map(String::as_str),
        Some("rw,size=64m")
    );
}

#[test]
fn weird_spec_preserves_port_bindings() {
    let host = load_weird_spec()
        .to_create_body(NEW_IMAGE)
        .host_config
        .expect("host_config must round-trip");

    let bindings = host.port_bindings.expect("port_bindings must round-trip");

    let https = bindings
        .get("8443/tcp")
        .and_then(|v| v.as_ref())
        .expect("8443/tcp binding must round-trip");
    assert_eq!(https.len(), 1);
    assert_eq!(https[0].host_ip.as_deref(), Some("127.0.0.1"));
    assert_eq!(https[0].host_port.as_deref(), Some("18443"));

    let metrics = bindings
        .get("9090/tcp")
        .and_then(|v| v.as_ref())
        .expect("9090/tcp binding must round-trip");
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].host_port.as_deref(), Some("19090"));
}

#[test]
fn weird_spec_preserves_cap_add_drop() {
    let host = load_weird_spec()
        .to_create_body(NEW_IMAGE)
        .host_config
        .expect("host_config must round-trip");

    assert_eq!(
        host.cap_add.as_deref(),
        Some(&["NET_BIND_SERVICE".to_owned(), "SYS_TIME".to_owned()][..]),
        "cap_add order must round-trip"
    );
    assert_eq!(
        host.cap_drop.as_deref(),
        Some(&["MKNOD".to_owned(), "AUDIT_WRITE".to_owned()][..]),
        "cap_drop order must round-trip"
    );
}

#[test]
fn weird_spec_preserves_sysctls() {
    let sysctls = load_weird_spec()
        .to_create_body(NEW_IMAGE)
        .host_config
        .expect("host_config must round-trip")
        .sysctls
        .expect("sysctls must round-trip");

    assert_eq!(
        sysctls
            .get("net.ipv4.ip_unprivileged_port_start")
            .map(String::as_str),
        Some("0")
    );
    assert_eq!(
        sysctls.get("net.core.somaxconn").map(String::as_str),
        Some("4096")
    );
}

#[test]
fn weird_spec_preserves_memory_and_nano_cpus() {
    let host = load_weird_spec()
        .to_create_body(NEW_IMAGE)
        .host_config
        .expect("host_config must round-trip");

    assert_eq!(host.memory, Some(134_217_728));
    assert_eq!(host.memory_reservation, Some(67_108_864));
    assert_eq!(
        host.nano_cpus,
        Some(500_000_000),
        "0.5 CPU expressed as nanocpus must round-trip"
    );
    assert_eq!(host.pids_limit, Some(256));
}

#[test]
fn weird_spec_preserves_restart_policy() {
    let policy = load_weird_spec()
        .to_create_body(NEW_IMAGE)
        .host_config
        .expect("host_config must round-trip")
        .restart_policy
        .expect("restart_policy must round-trip");

    assert_eq!(
        policy.name,
        Some(bollard::models::RestartPolicyNameEnum::ON_FAILURE)
    );
    assert_eq!(
        policy.maximum_retry_count,
        Some(5),
        "MaximumRetryCount must round-trip alongside the policy name"
    );
}

#[test]
fn weird_spec_preserves_stop_signal_and_timeout() {
    let body = load_weird_spec().to_create_body(NEW_IMAGE);
    assert_eq!(body.stop_signal.as_deref(), Some("SIGUSR1"));
    assert_eq!(body.stop_timeout, Some(45));
}

#[test]
fn weird_spec_preserves_multi_network_endpoints_with_aliases() {
    let endpoints = load_weird_spec()
        .to_create_body(NEW_IMAGE)
        .networking_config
        .expect("networking_config must round-trip")
        .endpoints_config
        .expect("endpoints_config must round-trip");

    let front = endpoints
        .get("fd-front")
        .expect("fd-front endpoint must round-trip");
    assert!(
        front
            .aliases
            .as_ref()
            .is_some_and(|a| a.iter().any(|s| s == "weird-front")),
        "fd-front aliases must round-trip"
    );
    assert_eq!(
        front
            .ipam_config
            .as_ref()
            .and_then(|c| c.ipv4_address.as_deref()),
        Some("172.31.10.20"),
        "static IP on fd-front must round-trip"
    );

    let back = endpoints
        .get("fd-back")
        .expect("fd-back endpoint must round-trip — multi-network attach is preserved");
    assert!(
        back.aliases
            .as_ref()
            .is_some_and(|a| a.iter().any(|s| s == "weird-back")),
        "fd-back aliases must round-trip independently of fd-front"
    );
}

#[test]
fn weird_spec_preserves_freshdock_and_user_labels_together() {
    let labels = load_weird_spec()
        .to_create_body(NEW_IMAGE)
        .labels
        .expect("labels must round-trip");

    // freshdock.* labels (managed namespace, Phase 3 rollback relies on these)
    assert_eq!(
        labels.get("freshdock.enable").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        labels.get("freshdock.mode").map(String::as_str),
        Some("watch")
    );
    assert_eq!(
        labels.get("freshdock.notify").map(String::as_str),
        Some("true")
    );
    // user labels (must survive untouched alongside freshdock.*)
    assert_eq!(labels.get("app").map(String::as_str), Some("weird"));
    assert_eq!(labels.get("team").map(String::as_str), Some("platform"));
    assert_eq!(
        labels.get("owner").map(String::as_str),
        Some("thijs@bendy.nl")
    );
}
