//! Docker credential configuration. Secrets deliberately have no Debug implementation.
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

#[derive(Clone, Default)]
pub struct Credential {
    pub username: String,
    pub secret: String,
    pub identity_token: bool,
}
#[derive(Default, Deserialize)]
struct DockerConfig {
    #[serde(default, rename = "credHelpers")]
    helpers: BTreeMap<String, String>,
    #[serde(default, rename = "credsStore")]
    store: String,
    #[serde(default)]
    auths: BTreeMap<String, InlineAuth>,
}
#[derive(Default, Deserialize)]
struct InlineAuth {
    #[serde(default)]
    auth: String,
    #[serde(default, rename = "identitytoken", alias = "identityToken")]
    identity_token: String,
}
pub struct Credentials {
    config: DockerConfig,
}
impl Credentials {
    pub fn load() -> Result<Self> {
        let directory = match std::env::var_os("DOCKER_CONFIG") {
            Some(path) => {
                ensure!(!path.is_empty(), "DOCKER_CONFIG must not be empty");
                PathBuf::from(path)
            }
            None => std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|p| PathBuf::from(p).join(".docker"))
                .context("home directory is unset; set DOCKER_CONFIG")?,
        };
        Self::from_path(&directory.join("config.json"))
    }
    pub fn from_path(path: &std::path::Path) -> Result<Self> {
        let config = match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid Docker credential configuration"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DockerConfig::default(),
            Err(_) => bail!("cannot read Docker credential configuration"),
        };
        Ok(Self { config })
    }
    pub fn get(&self, registry: &str) -> Result<Credential> {
        let keys = registry_keys(registry);
        if let Some((key, helper)) = keys
            .iter()
            .find_map(|key| self.config.helpers.get(key).map(|helper| (key, helper)))
        {
            return helper_get(helper, key);
        }
        if !self.config.store.is_empty() {
            return helper_get(&self.config.store, &keys[0]);
        }
        if let Some(auth) = keys.iter().find_map(|key| self.config.auths.get(key)) {
            if !auth.identity_token.is_empty() {
                return Ok(Credential {
                    secret: auth.identity_token.clone(),
                    identity_token: true,
                    ..Credential::default()
                });
            }
            if !auth.auth.is_empty() {
                let bytes = STANDARD
                    .decode(&auth.auth)
                    .map_err(|_| anyhow::anyhow!("invalid Docker inline credentials"))?;
                let decoded = String::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!("invalid Docker inline credentials"))?;
                let (username, secret) = decoded
                    .split_once(':')
                    .context("invalid Docker inline credentials")?;
                return Ok(Credential {
                    username: username.into(),
                    secret: secret.into(),
                    identity_token: false,
                });
            }
        }
        Ok(Credential::default())
    }
}
fn registry_keys(registry: &str) -> Vec<String> {
    if matches!(
        registry,
        "registry-1.docker.io" | "docker.io" | "index.docker.io"
    ) {
        vec![
            "https://index.docker.io/v1/".into(),
            "docker.io".into(),
            "registry-1.docker.io".into(),
            "index.docker.io".into(),
        ]
    } else {
        vec![
            registry.into(),
            format!("https://{registry}"),
            format!("https://{registry}/v1/"),
            format!("http://{registry}"),
            format!("http://{registry}/v1/"),
        ]
    }
}
fn helper_get(helper: &str, registry: &str) -> Result<Credential> {
    ensure!(
        !helper.is_empty()
            && helper
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
        "invalid Docker credential helper name"
    );
    let mut child = Command::new(format!("docker-credential-{helper}"))
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| anyhow::anyhow!("could not launch Docker credential helper"))?;
    child
        .stdin
        .take()
        .context("credential helper stdin unavailable")?
        .write_all(format!("{registry}\n").as_bytes())?;
    let output = child
        .wait_with_output()
        .context("waiting for Docker credential helper")?;
    if !output.status.success() {
        // The protocol specifies this sentinel for a missing entry. Other failures
        // must not silently become anonymous requests. Never print helper output.
        if String::from_utf8_lossy(&output.stdout).trim()
            == "credentials not found in native keychain"
            || String::from_utf8_lossy(&output.stderr).trim()
                == "credentials not found in native keychain"
        {
            return Ok(Credential::default());
        }
        bail!("Docker credential helper failed");
    }
    #[derive(Deserialize)]
    struct Entry {
        #[serde(rename = "Username")]
        username: String,
        #[serde(rename = "Secret")]
        secret: String,
    }
    let entry: Entry = serde_json::from_slice(&output.stdout)
        .map_err(|_| anyhow::anyhow!("invalid Docker credential helper response"))?;
    Ok(Credential {
        identity_token: entry.username == "<token>",
        username: entry.username,
        secret: entry.secret,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inline_auth_and_dockerhub_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"auths":{"https://index.docker.io/v1/":{"auth":"dXNlcjpzZWNyZXQ6d2l0aDpjb2xvbg=="}}}"#).unwrap();
        let credential = Credentials::from_path(&path)
            .unwrap()
            .get("registry-1.docker.io")
            .unwrap();
        assert_eq!(credential.username, "user");
        assert_eq!(credential.secret, "secret:with:colon");
        assert!(!credential.identity_token);
    }
    #[test]
    fn missing_config_is_anonymous_but_bad_config_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        assert!(
            Credentials::from_path(&path)
                .unwrap()
                .get("example.com")
                .unwrap()
                .secret
                .is_empty()
        );
        fs::write(&path, "{").unwrap();
        assert!(Credentials::from_path(&path).is_err());
        fs::write(&path, r#"{"auths":{"example.com":"credential-secret"}}"#).unwrap();
        let error = Credentials::from_path(&path).err().unwrap();
        assert!(!format!("{error:#}").contains("credential-secret"));
    }
}
