//! The only test in this file: it sets process-global `SSL_CERT_*` vars.

use freshdock::http::{self, HttpError};

#[test]
fn an_empty_root_store_is_reported_with_the_fix() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("empty-ca-store");
    std::fs::create_dir_all(&dir).expect("temp cert dir");

    // rustls-native-certs reads only these when set, so the store comes back empty.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", dir.join("nonexistent-bundle.crt"));
        std::env::set_var("SSL_CERT_DIR", &dir);
    }

    let err = http::client().expect_err("an empty root store must fail the build");
    assert!(matches!(err, HttpError::NoCaStore { .. }), "{err}");
    let rendered = err.to_string();
    assert!(rendered.contains("ca-certificates.crt"), "{rendered}");
    assert!(rendered.contains("SSL_CERT_FILE"), "{rendered}");
}
