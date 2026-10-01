#[allow(dead_code)]
mod support;
use layerlock::{
    credentials::Credentials,
    registry::{ImageRegistry, Registry, lookup_all},
};
use std::{
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::{Response, Server};
fn registry(config: &std::path::Path) -> Registry {
    Registry::new(
        Duration::from_secs(2),
        Credentials::from_path(config).unwrap(),
    )
    .unwrap()
}
#[test]
fn head_fallback_and_missing_are_manifest_only() {
    let requests = Arc::new(Mutex::new(vec![]));
    let seen = requests.clone();
    let server = Server::new(move |request| {
        seen.lock().unwrap().push((
            request.method.clone(),
            request.path.clone(),
            request.headers.clone(),
        ));
        if request.method == "HEAD" {
            Response::status(405)
        } else {
            Response::status(200).body("manifest, not layers")
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let registry = registry(&dir.path().join("missing"));
    assert!(registry.exists(&server.reference()).unwrap());
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].0, "HEAD");
    assert_eq!(requests[1].0, "GET");
    for (_, path, headers) in requests.iter() {
        assert!(path.starts_with("/v2/team/base/manifests/"));
        assert!(headers["accept"].contains("application/vnd.oci.image.index"));
        assert!(headers["accept"].contains("application/vnd.docker.distribution.manifest.list"));
    }
}
#[test]
fn bearer_exchange_uses_docker_credentials_and_reuses_tokens() {
    let token_requests = Arc::new(AtomicUsize::new(0));
    let count = token_requests.clone();
    let tokens = Server::new(move |request| {
        count.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.headers["authorization"], "Basic dXNlcjpzZWNyZXQ=");
        assert!(
            request
                .path
                .contains("scope=repository%3Ateam%2Fbase%3Apull")
        );
        Response::status(200)
            .header("Content-Type", "application/json")
            .body(r#"{"token":"test-token"}"#)
    });
    let realm = format!("http://{}/token", tokens.host);
    let server = Server::new(move |request| {
        if request
            .headers
            .get("authorization")
            .is_some_and(|s| s == "Bearer test-token")
        {
            Response::status(200)
        } else {
            Response::status(401).header(
                "WWW-Authenticate",
                &format!("Bearer realm=\"{realm}\",service=\"test\""),
            )
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    fs::write(
        &path,
        serde_json::to_vec(
            &serde_json::json!({"auths":{&server.host:{"auth":"dXNlcjpzZWNyZXQ="}}}),
        )
        .unwrap(),
    )
    .unwrap();
    let registry = registry(&path);
    assert!(registry.exists(&server.reference()).unwrap());
    assert!(registry.exists(&server.reference()).unwrap());
    assert_eq!(token_requests.load(Ordering::SeqCst), 1);
}
#[test]
fn identity_token_refresh_and_authenticated_not_found() {
    let tokens = Server::new(|request| {
        assert_eq!(request.method, "POST");
        assert!(request.body.contains("grant_type=refresh_token"));
        assert!(request.body.contains("refresh_token=identity-secret"));
        Response::status(200)
            .header("Content-Type", "application/json")
            .body(r#"{"access_token":"access"}"#)
    });
    let realm = format!("http://{}/token", tokens.host);
    let server = Server::new(move |request| {
        if request
            .headers
            .get("authorization")
            .is_some_and(|s| s == "Bearer access")
        {
            Response::status(404)
        } else {
            Response::status(401).header("WWW-Authenticate", &format!("Bearer realm=\"{realm}\""))
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    fs::write(
        &path,
        serde_json::to_vec(
            &serde_json::json!({"auths":{&server.host:{"identitytoken":"identity-secret"}}}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(!registry(&path).exists(&server.reference()).unwrap());
}
#[test]
fn basic_authentication_and_operational_statuses() {
    let server = Server::new(|request| {
        if request
            .headers
            .get("authorization")
            .is_some_and(|s| s == "Basic dXNlcjpzZWNyZXQ=")
        {
            Response::status(200)
        } else {
            Response::status(401).header("WWW-Authenticate", "Basic realm=\"test\"")
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    fs::write(
        &path,
        serde_json::to_vec(
            &serde_json::json!({"auths":{&server.host:{"auth":"dXNlcjpzZWNyZXQ="}}}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(registry(&path).exists(&server.reference()).unwrap());
    for status in [401, 403, 429, 500, 302] {
        let server =
            Server::new(move |_| Response::status(status).body("secret-token-must-not-appear"));
        let error = registry(&path)
            .exists(&server.reference())
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret-token"));
    }
    let server = Server::new(|_| Response::status(404));
    assert!(!registry(&path).exists(&server.reference()).unwrap());
}
#[test]
fn request_timeout_applies_and_error_is_sanitized() {
    let server = Server::new(|_| {
        std::thread::sleep(Duration::from_millis(100));
        Response::status(200)
    });
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(
        Duration::from_millis(20),
        Credentials::from_path(&dir.path().join("missing")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        registry
            .exists(&server.reference())
            .unwrap_err()
            .to_string(),
        "registry HTTP request timed out"
    );
}
#[test]
fn token_timeout_and_expired_token_refresh() {
    let count = Arc::new(AtomicUsize::new(0));
    let hits = count.clone();
    let tokens = Server::new(move |_| {
        let index = hits.fetch_add(1, Ordering::SeqCst);
        Response::status(200)
            .header("Content-Type", "application/json")
            .body(&format!("{{\"token\":\"token-{index}\"}}"))
    });
    let realm = format!("http://{}/token", tokens.host);
    let expected = Arc::new(AtomicUsize::new(0));
    let wanted = expected.clone();
    let server = Server::new(move |request| {
        if request.headers.get("authorization")
            == Some(&format!("Bearer token-{}", wanted.load(Ordering::SeqCst)))
        {
            Response::status(200)
        } else {
            Response::status(401).header("WWW-Authenticate", &format!("Bearer realm=\"{realm}\""))
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let registry = registry(&dir.path().join("missing"));
    assert!(registry.exists(&server.reference()).unwrap());
    expected.store(1, Ordering::SeqCst);
    assert!(registry.exists(&server.reference()).unwrap());
    assert_eq!(count.load(Ordering::SeqCst), 2);

    let slow = Server::new(|_| {
        std::thread::sleep(Duration::from_millis(100));
        Response::status(200)
    });
    let realm = format!("http://{}/token", slow.host);
    let server = Server::new(move |_| {
        Response::status(401).header("WWW-Authenticate", &format!("Bearer realm=\"{realm}\""))
    });
    let registry = Registry::new(
        Duration::from_millis(30),
        Credentials::from_path(&dir.path().join("missing")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        registry
            .exists(&server.reference())
            .unwrap_err()
            .to_string(),
        "registry HTTP request timed out"
    );
}
#[test]
fn concurrency_is_bounded() {
    let active = Arc::new(AtomicUsize::new(0));
    let max = Arc::new(AtomicUsize::new(0));
    let a = active.clone();
    let m = max.clone();
    let server = Server::new(move |_| {
        let current = a.fetch_add(1, Ordering::SeqCst) + 1;
        m.fetch_max(current, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        a.fetch_sub(1, Ordering::SeqCst);
        Response::status(200)
    });
    let dir = tempfile::tempdir().unwrap();
    let registry = registry(&dir.path().join("missing"));
    let references: Vec<_> = (0..6)
        .map(|i| format!("{}/team/base:tag-{i}", server.host))
        .collect();
    let results = lookup_all(&registry, &references, 2);
    assert_eq!(results.len(), 6);
    assert!(results.values().all(|r| matches!(r, Ok(true))));
    assert!(max.load(Ordering::SeqCst) <= 2);
    assert!(max.load(Ordering::SeqCst) > 1);
}
