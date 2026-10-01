use crate::credentials::{Credential, Credentials};
use anyhow::{Context, Result, bail, ensure};
use reqwest::{
    Method, StatusCode, Url,
    blocking::{Client, Response},
    header::{ACCEPT, WWW_AUTHENTICATE},
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const ACCEPT_MANIFESTS: &str = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json";

pub trait ImageRegistry: Sync {
    fn exists(&self, reference: &str) -> Result<bool>;
}
#[derive(Debug, Clone)]
pub struct ImageReference {
    pub registry: String,
    pub repository: String,
    pub tag: String,
}
impl ImageReference {
    pub fn parse(reference: &str) -> Result<Self> {
        let (name, tag) = reference
            .rsplit_once(':')
            .context("image reference requires a tag")?;
        ensure!(
            !tag.is_empty()
                && tag.len() <= 128
                && tag
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'),
            "invalid image tag"
        );
        let (first, rest) = name.split_once('/').unwrap_or((name, ""));
        let explicit_host = !rest.is_empty()
            && (first.contains('.')
                || first.contains(':')
                || first == "localhost"
                || first.starts_with('['));
        let (mut registry, mut repository) = if explicit_host {
            (first.to_string(), rest.to_string())
        } else {
            ("registry-1.docker.io".into(), name.to_string())
        };
        if matches!(registry.as_str(), "docker.io" | "index.docker.io") {
            registry = "registry-1.docker.io".into();
        }
        if registry == "registry-1.docker.io" && !repository.contains('/') {
            repository = format!("library/{repository}");
        }
        ensure!(
            repository.split('/').all(|part| !part.is_empty()
                && part.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                && part.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                && part.chars().all(|c| c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || c == '.'
                    || c == '_'
                    || c == '-')),
            "invalid image repository"
        );
        let url = Url::parse(&format!("https://{registry}/")).context("invalid registry host")?;
        ensure!(
            url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "invalid registry host"
        );
        registry = url.authority().to_string();
        Ok(Self {
            registry,
            repository,
            tag: tag.into(),
        })
    }
    pub fn manifest_url(&self) -> Result<Url> {
        let https = Url::parse(&format!("https://{}/", self.registry))?;
        let scheme = if loopback(&https) { "http" } else { "https" };
        Ok(Url::parse(&format!(
            "{scheme}://{}/v2/{}/manifests/{}",
            self.registry, self.repository, self.tag
        ))?)
    }
}
fn loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host == "[::1]"
            || host == "::1"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

pub struct Registry {
    client: Client,
    credentials: Credentials,
    credential_cache: Mutex<BTreeMap<String, Credential>>,
    tokens: Mutex<BTreeMap<String, String>>,
}
impl Registry {
    pub fn new(timeout: Duration, credentials: Credentials) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("layerlock/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|_| anyhow::anyhow!("creating registry HTTP client failed"))?,
            credentials,
            credential_cache: Mutex::new(BTreeMap::new()),
            tokens: Mutex::new(BTreeMap::new()),
        })
    }
    fn credential(&self, host: &str) -> Result<Credential> {
        let mut cache = self.credential_cache.lock().unwrap();
        if !cache.contains_key(host) {
            cache.insert(host.into(), self.credentials.get(host)?);
        }
        Ok(cache[host].clone())
    }
    fn request(
        &self,
        method: Method,
        image: &ImageReference,
        token: Option<&str>,
        basic: Option<&Credential>,
    ) -> Result<Response> {
        let mut request = self
            .client
            .request(method, image.manifest_url()?)
            .header(ACCEPT, ACCEPT_MANIFESTS);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(credential) = basic {
            request = request.basic_auth(&credential.username, Some(&credential.secret));
        }
        request.send().map_err(http_error)
    }
    fn lookup(&self, method: Method, image: &ImageReference) -> Result<Response> {
        let scope_key = format!("{}/{}", image.registry, image.repository);
        let cached = self.tokens.lock().unwrap().get(&scope_key).cloned();
        let response = self.request(method.clone(), image, cached.as_deref(), None)?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let challenges: Vec<_> = response
            .headers()
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .filter_map(|h| h.to_str().ok())
            .flat_map(split_challenges)
            .collect();
        let challenge = challenges
            .iter()
            .find(|h| h.to_ascii_lowercase().starts_with("bearer "))
            .or_else(|| {
                challenges
                    .iter()
                    .find(|h| h.to_ascii_lowercase().starts_with("basic "))
            })
            .context("registry authentication failed: no supported challenge")?;
        let (scheme, params) = parse_challenge(challenge)?;
        let credential = self.credential(&image.registry)?;
        if scheme == "basic" {
            ensure!(
                !credential.secret.is_empty() && !credential.identity_token,
                "registry requires Docker login credentials"
            );
            return self.request(method, image, None, Some(&credential));
        }
        // Serialize acquisition and reuse tokens for simultaneous duplicate scopes.
        // A cached token that was rejected must be replaced, never retried forever.
        let mut tokens = self.tokens.lock().unwrap();
        let token = if let Some(token) = tokens
            .get(&scope_key)
            .filter(|token| Some(*token) != cached.as_ref())
        {
            token.clone()
        } else {
            let token = self.acquire_token(image, &params, &credential)?;
            tokens.insert(scope_key, token.clone());
            token
        };
        drop(tokens);
        self.request(method, image, Some(&token), None)
    }
    fn acquire_token(
        &self,
        image: &ImageReference,
        params: &BTreeMap<String, String>,
        credential: &Credential,
    ) -> Result<String> {
        let realm = params
            .get("realm")
            .context("Bearer challenge has no realm")?;
        let url = Url::parse(realm).map_err(|_| anyhow::anyhow!("invalid token endpoint"))?;
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
                && (url.scheme() == "https"
                    || (url.scheme() == "http"
                        && loopback(&url)
                        && image.manifest_url()?.scheme() == "http")),
            "token endpoint must use HTTPS (HTTP permitted only on loopback)"
        );
        let scope = params
            .get("scope")
            .cloned()
            .unwrap_or_else(|| format!("repository:{}:pull", image.repository));
        let service = params
            .get("service")
            .map(String::as_str)
            .unwrap_or(&image.registry);
        let mut request = if credential.identity_token {
            self.client.post(url).form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", credential.secret.as_str()),
                ("service", service),
                ("scope", scope.as_str()),
                ("client_id", "layerlock"),
            ])
        } else {
            let mut request = self
                .client
                .get(url)
                .query(&[("service", service), ("scope", scope.as_str())]);
            if !credential.secret.is_empty() {
                request = request.basic_auth(&credential.username, Some(&credential.secret));
            }
            request
        };
        request = request.header(ACCEPT, "application/json");
        let response = request.send().map_err(http_error)?;
        ensure!(
            response.status().is_success(),
            "registry token request failed (HTTP {})",
            response.status().as_u16()
        );
        #[derive(Deserialize)]
        struct Token {
            token: Option<String>,
            access_token: Option<String>,
        }
        let token: Token = response
            .json()
            .map_err(|_| anyhow::anyhow!("invalid registry token response"))?;
        let token = token
            .token
            .filter(|s| !s.is_empty())
            .or(token.access_token)
            .context("registry token response contains no token")?;
        ensure!(!token.is_empty(), "registry returned an empty token");
        Ok(token)
    }
}
fn http_error(error: reqwest::Error) -> anyhow::Error {
    // reqwest errors contain URLs, including token query parameters. Never expose them.
    anyhow::anyhow!(if error.is_timeout() {
        "registry HTTP request timed out"
    } else {
        "registry HTTP request failed"
    })
}
impl ImageRegistry for Registry {
    fn exists(&self, reference: &str) -> Result<bool> {
        let image = ImageReference::parse(reference)?;
        let mut response = self.lookup(Method::HEAD, &image)?;
        if matches!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
        ) {
            response = self.lookup(Method::GET, &image)?;
        }
        match response.status() {
            StatusCode::OK => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            status => bail!("registry manifest lookup failed (HTTP {})", status.as_u16()),
        }
    }
}

// Some servers combine multiple challenges into a single header. Split only
// commas outside quoted parameters that introduce another supported scheme.
fn split_challenges(header: &str) -> Vec<&str> {
    let mut starts = vec![0];
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in header.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '"' {
            quoted = !quoted;
        }
        if ch == ',' && !quoted {
            let tail = header[index + 1..].trim_start();
            let lower = tail.to_ascii_lowercase();
            if lower.starts_with("bearer ") || lower.starts_with("basic ") {
                starts.push(index + 1);
            }
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            header[*start
                ..starts
                    .get(index + 1)
                    .map(|end| end - 1)
                    .unwrap_or(header.len())]
                .trim()
        })
        .collect()
}

/// RFC-style quoted auth parameters (commas inside quoted values are not separators).
fn parse_challenge(challenge: &str) -> Result<(String, BTreeMap<String, String>)> {
    let (scheme, mut rest) = challenge
        .split_once(' ')
        .context("invalid registry authentication challenge")?;
    let mut params = BTreeMap::new();
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ',');
        if rest.is_empty() {
            break;
        }
        let (key, value) = rest
            .split_once('=')
            .context("invalid registry authentication challenge")?;
        let key = key.trim().to_ascii_lowercase();
        ensure!(
            !key.is_empty() && !key.contains(' '),
            "invalid registry authentication challenge"
        );
        rest = value.trim_start();
        let mut parsed = String::new();
        if let Some(quoted) = rest.strip_prefix('"') {
            let mut escaped = false;
            let mut end = None;
            for (index, ch) in quoted.char_indices() {
                if escaped {
                    parsed.push(ch);
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    end = Some(index + 1);
                    break;
                } else {
                    parsed.push(ch);
                }
            }
            rest = &quoted[end.context("unterminated registry authentication parameter")?..];
            ensure!(
                rest.is_empty() || rest.starts_with(',') || rest.starts_with(char::is_whitespace),
                "invalid registry authentication challenge"
            );
        } else {
            let (value, tail) = rest.split_once(',').unwrap_or((rest, ""));
            parsed = value.trim().into();
            rest = tail;
        }
        params.insert(key, parsed);
    }
    Ok((scheme.to_ascii_lowercase(), params))
}

pub fn lookup_all(
    registry: &dyn ImageRegistry,
    references: &[String],
    concurrency: usize,
) -> BTreeMap<String, Result<bool>> {
    let next = AtomicUsize::new(0);
    let results = Mutex::new(BTreeMap::new());
    std::thread::scope(|scope| {
        for _ in 0..concurrency.min(references.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(reference) = references.get(index) else {
                        break;
                    };
                    let result = registry
                        .exists(reference)
                        .with_context(|| format!("checking {reference}"));
                    results.lock().unwrap().insert(reference.clone(), result);
                }
            });
        }
    });
    results.into_inner().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reference_normalization() {
        let image = ImageReference::parse("python:tag").unwrap();
        assert_eq!(image.registry, "registry-1.docker.io");
        assert_eq!(image.repository, "library/python");
        assert_eq!(
            ImageReference::parse("docker.io/python:tag")
                .unwrap()
                .repository,
            "library/python"
        );
        assert_eq!(
            ImageReference::parse("localhost:5000/team/base:tag")
                .unwrap()
                .manifest_url()
                .unwrap()
                .scheme(),
            "http"
        );
        for reference in [
            "foo",
            "host/a:bad/tag",
            "host/UPPER:tag",
            "https://host/a:tag",
            "host//a:tag",
        ] {
            assert!(ImageReference::parse(reference).is_err(), "{reference}");
        }
    }
    #[test]
    fn challenge_quoted_commas_and_escapes() {
        let (_, params) = parse_challenge(
            r#"Bearer realm="https://auth/token",service="a",scope="repository:a:pull,push""#,
        )
        .unwrap();
        assert_eq!(params["scope"], "repository:a:pull,push");
        assert!(parse_challenge("Bearer realm=\"unterminated").is_err());
        assert_eq!(
            split_challenges(r#"Basic realm="x,y", Bearer realm="https://auth",scope="pull,push""#),
            [
                r#"Basic realm="x,y""#,
                r#"Bearer realm="https://auth",scope="pull,push""#
            ]
        );
    }
}
