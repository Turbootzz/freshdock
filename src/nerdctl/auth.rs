//! Registry credentials for one `nerdctl pull`.
//!
//! nerdctl reads credentials from a Docker-style `config.json` in the
//! directory `DOCKER_CONFIG` names. For a pull that has freshdock credentials,
//! [`PullAuth`] writes such a directory with only that registry in it, readable
//! only by its owner, and deletes it when dropped. Without freshdock
//! credentials nerdctl's own login is used, as with `nerdctl pull` by hand.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::RegistryCredentials;

/// A temporary `DOCKER_CONFIG` directory, removed on drop.
#[derive(Debug)]
pub struct PullAuth {
    dir: PathBuf,
}

impl PullAuth {
    /// Write a config holding `credentials` for the canonical registry `host`.
    pub fn write(host: &str, credentials: &RegistryCredentials) -> std::io::Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir =
            std::env::temp_dir().join(format!("freshdock-pull-{}-{nanos}", std::process::id()));
        create_private_dir(&dir)?;
        let auth = PullAuth { dir };
        let user = credentials.username.as_deref().unwrap_or("");
        let token = credentials.token.expose();
        let body = serde_json::json!({
            "auths": { config_key(host): { "auth": base64(format!("{user}:{token}").as_bytes()) } }
        });
        let mut file = create_private_file(&auth.dir.join("config.json"))?;
        file.write_all(body.to_string().as_bytes())?;
        Ok(auth)
    }

    /// The value for `DOCKER_CONFIG`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for PullAuth {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Docker Hub's credentials live under its legacy index URL; every other
/// registry under its host.
fn config_key(host: &str) -> String {
    if host == "docker.io" {
        "https://index.docker.io/v1/".to_owned()
    } else {
        host.to_owned()
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dir)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Standard base64 with padding: the `auth` field's encoding.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(username: Option<&str>, token: &str) -> RegistryCredentials {
        let toml = match username {
            Some(u) => format!("username = \"{u}\"\ntoken = \"{token}\"\n"),
            None => format!("token = \"{token}\"\n"),
        };
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain:?}");
        }
    }

    #[test]
    fn writes_a_private_config_for_one_registry_and_cleans_up() {
        let auth = PullAuth::write("ghcr.io", &creds(Some("me"), "s3cret")).unwrap();
        let dir = auth.dir().to_owned();
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(config["auths"]["ghcr.io"]["auth"], base64(b"me:s3cret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join("config.json")), 0o600);
        }
        drop(auth);
        assert!(!dir.exists(), "the directory is removed on drop");
    }

    #[test]
    fn docker_hub_uses_the_legacy_index_key_and_a_missing_username_is_empty() {
        let auth = PullAuth::write("docker.io", &creds(None, "tok")).unwrap();
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(auth.dir().join("config.json")).unwrap())
                .unwrap();
        assert_eq!(
            config["auths"]["https://index.docker.io/v1/"]["auth"],
            base64(b":tok")
        );
    }
}
