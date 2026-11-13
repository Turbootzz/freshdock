//! Registry credentials for one `nerdctl pull`.
//!
//! nerdctl reads credentials from a Docker-style `config.json` in the
//! directory `DOCKER_CONFIG` names. For a pull that has freshdock credentials,
//! [`PullAuth`] writes such a directory with only that registry in it, readable
//! only by its owner, and deletes it when dropped. Without freshdock
//! credentials nerdctl's own login is used, as with `nerdctl pull` by hand.

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::private::PrivateDir;
use crate::config::RegistryCredentials;

/// A temporary `DOCKER_CONFIG` directory, removed on drop.
#[derive(Debug)]
pub struct PullAuth {
    dir: PrivateDir,
}

impl PullAuth {
    /// Write a config holding `credentials` for the canonical registry `host`.
    pub fn write(host: &str, credentials: &RegistryCredentials) -> std::io::Result<Self> {
        let dir = PrivateDir::new("pull")?;
        let user = credentials.username.as_deref().unwrap_or("");
        let token = credentials.token.expose();
        let body = serde_json::json!({
            "auths": { config_key(host): { "auth": STANDARD.encode(format!("{user}:{token}")) } }
        });
        dir.write("config.json", body.to_string().as_bytes())?;
        Ok(PullAuth { dir })
    }

    /// The value for `DOCKER_CONFIG`.
    pub fn dir(&self) -> &Path {
        self.dir.path()
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

    fn config(auth: &PullAuth) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(auth.dir().join("config.json")).unwrap())
            .unwrap()
    }

    #[test]
    fn writes_a_private_config_for_one_registry_and_cleans_up() {
        let auth = PullAuth::write("ghcr.io", &creds(Some("me"), "s3cret")).unwrap();
        let dir = auth.dir().to_owned();
        // "me:s3cret"
        assert_eq!(config(&auth)["auths"]["ghcr.io"]["auth"], "bWU6czNjcmV0");
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
        // ":tok"
        assert_eq!(
            config(&auth)["auths"]["https://index.docker.io/v1/"]["auth"],
            "OnRvaw=="
        );
    }
}
