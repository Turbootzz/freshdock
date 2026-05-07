use bollard::models::ContainerInspectResponse;
use freshdock::docker::spec::ContainerSpec;

const FIXTURE: &str = include_str!("fixtures/container_inspect.json");
const NEW_IMAGE: &str =
    "nginx@sha256:2222222222222222222222222222222222222222222222222222222222222222";

fn load_inspect() -> ContainerInspectResponse {
    serde_json::from_str(FIXTURE).expect("fixture should deserialize into ContainerInspectResponse")
}

fn load_spec() -> ContainerSpec {
    ContainerSpec::from_inspect(load_inspect()).expect("spec should build from fixture")
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
