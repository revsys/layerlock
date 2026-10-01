#[allow(dead_code)]
mod support;
use std::{
    fs,
    process::{Command, Output},
};
use support::{Response, Server};

fn command() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_layerlock"));
    for key in [
        "LAYERLOCK_CONFIG",
        "LAYERLOCK_GROUPS",
        "LAYERLOCK_REGISTRY_TIMEOUT",
        "LAYERLOCK_REGISTRY_CONCURRENCY",
        "LAYERLOCK_OUTPUT",
        "LAYERLOCK_COLOR",
    ] {
        cmd.env_remove(key);
    }
    cmd
}
fn fixture(host: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("deps"), "dependencies\n").unwrap();
    fs::write(dir.path().join("base"), "FROM scratch\n").unwrap();
    fs::write(
        dir.path().join("Dockerfile"),
        "# layerlock: a\nFROM old AS app\n",
    )
    .unwrap();
    fs::write(dir.path().join(".layerlock.toml"), format!("dockerfiles = ['Dockerfile']\n[groups.a]\ndependencies = ['deps']\ndockerfile = 'base'\nrepository = '{host}/a'\n[groups.b]\ndependencies = ['deps']\ndockerfile = 'base'\nrepository = '{host}/b'\n")).unwrap();
    dir
}
fn configured(dir: &std::path::Path) -> Command {
    let mut command = command();
    command
        .current_dir(dir)
        .env("DOCKER_CONFIG", dir.join("credentials"));
    command
}
fn json(output: &Output, code: i32) -> serde_json::Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
#[test]
fn cli_selection_replaces_environment_and_check_emits_complete_read_only_plan() {
    let server = Server::new(|_| Response::status(200));
    let dir = fixture(&server.host);
    let output = configured(dir.path())
        .env("LAYERLOCK_GROUPS", "b")
        .env("LAYERLOCK_OUTPUT", "json")
        .args(["check", "--group", "a,a"])
        .output()
        .unwrap();
    let report = json(&output, 1);
    assert_eq!(report["selected_groups"], serde_json::json!(["a"]));
    assert_eq!(report["complete"], true);
    assert_eq!(report["work_needed"], true);
    assert_eq!(report["groups"][0]["image_action"], "reuse");
    assert_eq!(report["dockerfile_changes"].as_array().unwrap().len(), 1);
    assert!(report["errors"].as_array().unwrap().is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(dir.path().join("Dockerfile")).unwrap(),
        "# layerlock: a\nFROM old AS app\n"
    );
}
#[test]
fn explicit_options_override_invalid_environment_before_or_after_subcommand() {
    let server = Server::new(|_| Response::status(200));
    let dir = fixture(&server.host);
    for args in [
        vec!["--registry-timeout", "10", "check", "--output", "json"],
        vec!["check", "--registry-timeout", "10", "--output", "json"],
    ] {
        let output = configured(dir.path())
            .env("LAYERLOCK_REGISTRY_TIMEOUT", "bad")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(json(&output, 1)["complete"], true);
    }
    let output = configured(dir.path())
        .env("LAYERLOCK_GROUPS", "bad,")
        .args(["-g", "a", "check", "--output", "json"])
        .output()
        .unwrap();
    assert_eq!(
        json(&output, 1)["selected_groups"],
        serde_json::json!(["a"])
    );
    let output = configured(dir.path())
        .env("LAYERLOCK_GROUPS", "unknown,")
        .args(["-g", "a", "check", "-g", "b", "--output", "json"])
        .output()
        .unwrap();
    assert_eq!(
        json(&output, 1)["selected_groups"],
        serde_json::json!(["a", "b"])
    );
    let output = configured(dir.path())
        .env("LAYERLOCK_GROUPS", "b,b")
        .args(["check", "--output", "json"])
        .output()
        .unwrap();
    assert_eq!(
        json(&output, 0)["selected_groups"],
        serde_json::json!(["b"])
    );
}
#[test]
fn invalid_effective_environment_is_an_argument_error() {
    for (key, value) in [
        ("LAYERLOCK_REGISTRY_TIMEOUT", "0"),
        ("LAYERLOCK_CONFIG", ""),
        ("LAYERLOCK_GROUPS", "a,"),
        ("LAYERLOCK_OUTPUT", ""),
    ] {
        let output = command().env(key, value).arg("check").output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{key}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}
#[test]
fn missing_config_and_unknown_groups_have_structured_errors() {
    let dir = fixture("example.com");
    for args in [
        vec!["check", "-c", "missing", "--output", "json"],
        vec!["check", "-g", "missing", "--output", "json"],
    ] {
        let output = configured(dir.path()).args(args).output().unwrap();
        let report = json(&output, 2);
        assert_eq!(report["complete"], false);
        assert_eq!(report["work_needed"], serde_json::Value::Null);
        assert_eq!(report["errors"].as_array().unwrap().len(), 1);
    }
}
#[test]
fn help_lists_environment_variables() {
    let output = command().args(["check", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for variable in [
        "LAYERLOCK_CONFIG",
        "LAYERLOCK_GROUPS",
        "LAYERLOCK_REGISTRY_TIMEOUT",
        "LAYERLOCK_REGISTRY_CONCURRENCY",
        "LAYERLOCK_OUTPUT",
        "LAYERLOCK_COLOR",
    ] {
        assert!(help.contains(variable));
    }
    assert!(!help.contains("LAYERLOCK_FORCE"));
}
#[test]
fn successful_sync_then_check_noop_and_force_check_work() {
    let server = Server::new(|_| Response::status(200));
    let dir = fixture(&server.host);
    let output = configured(dir.path())
        .args(["sync", "--output", "json"])
        .output()
        .unwrap();
    let report = json(&output, 0);
    assert_eq!(report["complete"], true);
    assert_eq!(report["dockerfile_changes"][0]["completed"], true);
    let source = fs::read(dir.path().join("Dockerfile")).unwrap();
    let output = configured(dir.path())
        .args(["check", "--output", "json"])
        .output()
        .unwrap();
    let report = json(&output, 0);
    assert_eq!(report["work_needed"], false);
    let output = configured(dir.path())
        .args(["check", "--force", "--output", "json"])
        .output()
        .unwrap();
    let report = json(&output, 1);
    assert_eq!(report["work_needed"], true);
    assert_eq!(report["groups"][0]["image_action"], "force_build_and_push");
    assert_eq!(fs::read(dir.path().join("Dockerfile")).unwrap(), source);
}
#[test]
fn missing_image_without_diffs_still_needs_work_and_errors_take_precedence() {
    let status = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(200));
    let s = status.clone();
    let server =
        Server::new(move |_| Response::status(s.load(std::sync::atomic::Ordering::SeqCst)));
    let dir = fixture(&server.host);
    json(
        &configured(dir.path())
            .args(["sync", "--output", "json"])
            .output()
            .unwrap(),
        0,
    );
    status.store(404, std::sync::atomic::Ordering::SeqCst);
    let report = json(
        &configured(dir.path())
            .args(["check", "--output", "json"])
            .output()
            .unwrap(),
        1,
    );
    assert!(report["dockerfile_changes"].as_array().unwrap().is_empty());
    assert_eq!(report["groups"][0]["image_action"], "build_and_push");
    status.store(403, std::sync::atomic::Ordering::SeqCst);
    let report = json(
        &configured(dir.path())
            .args(["check", "--force", "--output", "json"])
            .output()
            .unwrap(),
        2,
    );
    assert_eq!(report["complete"], false);
    assert_eq!(report["work_needed"], true);
}

#[cfg(unix)]
fn executable(path: &std::path::Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
#[cfg(unix)]
fn fake_docker(dir: &std::path::Path) -> String {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    executable(
        &bin.join("docker"),
        r#"#!/bin/sh
printf '%s\n' BEGIN "$@" END >> "$ARG_LOG"
case "$1 $2" in
  "image inspect")
    if [ -n "$INSPECT_JSON" ]; then cat "$INSPECT_JSON"; exit 0; fi
    echo 'Error response from daemon: No such image: expected' >&2
    exit 1;;
  "image push"|"buildx build")
    echo 'build stdout log'; echo 'build stderr log' >&2
    if [ "$FAIL_BUILD" = 1 ]; then exit 7; fi
    touch "$PUBLISHED"; exit 0;;
esac
exit 9
"#,
    );
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}
#[cfg(unix)]
#[test]
fn buildx_execution_keeps_logs_off_stdout_verifies_publication_and_normalizes_exit_codes() {
    let dir = fixture("example.com");
    let published = dir.path().join("published");
    let marker = published.clone();
    let server = Server::new(move |_| Response::status(if marker.exists() { 200 } else { 404 }));
    // Exercise target, arguments, and multiple platforms through the real CLI.
    fs::write(dir.path().join(".layerlock.toml"), format!("dockerfiles = ['Dockerfile']\n[groups.a]\ndependencies = ['deps']\ndockerfile = 'base'\nrepository = '{}/a'\nbuild-args = {{ VERSION = '2' }}\ntarget = 'deps'\nplatforms = ['linux/arm64', 'linux/amd64']\n", server.host)).unwrap();
    let path = fake_docker(dir.path());
    let log = dir.path().join("arguments");
    let mut cmd = configured(dir.path());
    cmd.env("PATH", &path)
        .env("ARG_LOG", &log)
        .env("PUBLISHED", &published)
        .args(["check", "--output", "json"]);
    let report = json(&cmd.output().unwrap(), 1);
    assert_eq!(report["groups"][0]["image_action"], "build_and_push");
    assert!(!log.exists());
    assert!(!published.exists());
    let output = configured(dir.path())
        .env("PATH", &path)
        .env("ARG_LOG", &log)
        .env("PUBLISHED", &published)
        .args(["sync", "--output", "json"])
        .output()
        .unwrap();
    let report = json(&output, 0);
    assert_eq!(report["groups"][0]["completed"], true);
    assert!(String::from_utf8_lossy(&output.stderr).contains("build stdout log"));
    let arguments = fs::read_to_string(&log).unwrap();
    for value in [
        "buildx\nbuild\n--push\n",
        "--target\ndeps\n",
        "--platform\nlinux/amd64,linux/arm64\n",
        "--build-arg\nVERSION=2\n",
        "io.layerlock.input-digest=",
        "io.layerlock.hash-version=1",
    ] {
        assert!(arguments.contains(value), "{arguments}");
    }
    assert!(!arguments.contains("--pull"));
    assert!(!arguments.contains("--no-cache"));
    fs::write(
        dir.path().join("Dockerfile"),
        "# layerlock: a\nFROM stale\n",
    )
    .unwrap();
    let output = configured(dir.path())
        .env("PATH", &path)
        .env("ARG_LOG", &log)
        .env("PUBLISHED", &published)
        .env("FAIL_BUILD", "1")
        .args(["sync", "--force", "--output", "json"])
        .output()
        .unwrap();
    assert_eq!(json(&output, 2)["complete"], false);
    assert_eq!(
        fs::read_to_string(dir.path().join("Dockerfile")).unwrap(),
        "# layerlock: a\nFROM stale\n"
    );
}
#[cfg(unix)]
#[test]
fn local_provenance_is_inspected_then_pushed_without_building() {
    let dir = fixture("example.com");
    let published = dir.path().join("published");
    let marker = published.clone();
    let server = Server::new(move |_| Response::status(if marker.exists() { 200 } else { 404 }));
    let config_path = dir.path().join(".layerlock.toml");
    fs::write(&config_path, format!("dockerfiles = ['Dockerfile']\n[groups.a]\ndependencies = ['deps']\ndockerfile = 'base'\nrepository = '{}/a'\nplatforms = ['linux/amd64']\n", server.host)).unwrap();
    let loaded = layerlock::config::LoadedConfig::load(&config_path).unwrap();
    let digest = layerlock::hash::digest(&loaded.root, &loaded.config.groups["a"]).unwrap();
    let inspect = dir.path().join("inspect.json");
    fs::write(&inspect, serde_json::to_vec(&serde_json::json!([{"Os":"linux", "Architecture":"amd64", "Config":{"Labels":{
        "io.layerlock.input-digest":digest, "io.layerlock.hash-version":"1", "io.layerlock.platforms":"[\"linux/amd64\"]"
    }}}])).unwrap()).unwrap();
    let path = fake_docker(dir.path());
    let log = dir.path().join("arguments");
    let output = configured(dir.path())
        .env("PATH", &path)
        .env("ARG_LOG", &log)
        .env("PUBLISHED", &published)
        .env("INSPECT_JSON", &inspect)
        .args(["check", "--output", "json"])
        .output()
        .unwrap();
    assert_eq!(json(&output, 1)["groups"][0]["image_action"], "push_local");
    assert!(!published.exists());
    let output = configured(dir.path())
        .env("PATH", &path)
        .env("ARG_LOG", &log)
        .env("PUBLISHED", &published)
        .env("INSPECT_JSON", &inspect)
        .args(["sync", "--output", "json"])
        .output()
        .unwrap();
    assert_eq!(json(&output, 0)["groups"][0]["image_action"], "push_local");
    let arguments = fs::read_to_string(log).unwrap();
    assert!(arguments.contains("image\npush\n"));
    assert!(!arguments.contains("buildx"));
}
#[cfg(unix)]
#[test]
fn per_registry_helper_overrides_store_and_inline_auth_and_is_cached_without_secret_logs() {
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
    let dir = fixture(&server.host);
    let path = fake_docker(dir.path());
    let credentials = dir.path().join("credentials");
    fs::create_dir(&credentials).unwrap();
    fs::write(credentials.join("config.json"), serde_json::to_vec(&serde_json::json!({"credHelpers":{&server.host:"test"}, "credsStore":"nonexistent", "auths":{&server.host:{"auth":"invalid"}}})).unwrap()).unwrap();
    executable(
        &dir.path().join("bin/docker-credential-test"),
        r#"#!/bin/sh
read registry
printf '%s\n' "$registry" >> "$HELPER_LOG"
echo '{"Username":"user","Secret":"secret"}'
"#,
    );
    let log = dir.path().join("helper-log");
    let output = configured(dir.path())
        .env("PATH", &path)
        .env("HELPER_LOG", &log)
        .args(["check", "--output", "json"])
        .output()
        .unwrap();
    assert_eq!(json(&output, 1)["complete"], true);
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!("{}\n", server.host)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("secret"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret"));
    // With no per-registry override, the default store also beats inline auth.
    fs::write(
        credentials.join("config.json"),
        serde_json::to_vec(
            &serde_json::json!({"credsStore":"test", "auths":{&server.host:{"auth":"invalid"}}}),
        )
        .unwrap(),
    )
    .unwrap();
    let output = configured(dir.path())
        .env("PATH", &path)
        .env("HELPER_LOG", &log)
        .args(["check", "--output", "json"])
        .output()
        .unwrap();
    assert_eq!(json(&output, 1)["complete"], true);
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 2);
    executable(
        &dir.path().join("bin/docker-credential-test"),
        "#!/bin/sh\necho leaked-secret-value >&2\nexit 1\n",
    );
    let output = configured(dir.path())
        .env("PATH", &path)
        .env("HELPER_LOG", &log)
        .args(["check", "--output", "json"])
        .output()
        .unwrap();
    assert_eq!(json(&output, 2)["complete"], false);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("leaked-secret-value"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("leaked-secret-value"));
}
