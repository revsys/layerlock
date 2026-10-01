use anyhow::{Result, bail};
use layerlock::{
    cli::Command,
    config::{Group, LoadedConfig},
    docker::ImageBuilder,
    plan::{self, ImageAction, Report},
    registry::ImageRegistry,
};
use std::{collections::BTreeSet, fs, path::Path, sync::Mutex};

#[derive(Default)]
struct State {
    remote: BTreeSet<String>,
    local: bool,
    publish: bool,
    fail_build_number: Option<usize>,
    fail_lookups: bool,
    lookups: usize,
    inspections: usize,
    pushes: usize,
    builds: usize,
}
struct Services(Mutex<State>);
impl ImageRegistry for Services {
    fn exists(&self, reference: &str) -> Result<bool> {
        let mut state = self.0.lock().unwrap();
        state.lookups += 1;
        if state.fail_lookups {
            bail!("mock registry unavailable");
        }
        Ok(state.remote.contains(reference))
    }
}
impl ImageBuilder for Services {
    fn local_matches(&self, _: &str, _: &str, _: &Group) -> Result<bool> {
        let mut state = self.0.lock().unwrap();
        state.inspections += 1;
        Ok(state.local)
    }
    fn push(&self, reference: &str) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        state.pushes += 1;
        if state.publish {
            state.remote.insert(reference.into());
        }
        Ok(())
    }
    fn build_and_push(&self, _: &Path, reference: &str, _: &str, _: &Group) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        state.builds += 1;
        if state.fail_build_number == Some(state.builds) {
            bail!("mock build failed");
        }
        if state.publish {
            state.remote.insert(reference.into());
        }
        Ok(())
    }
}
fn fixture(deduplicate: bool) -> (tempfile::TempDir, LoadedConfig) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("deps"), "locked\n").unwrap();
    fs::write(dir.path().join("base"), "FROM scratch\n").unwrap();
    fs::write(
        dir.path().join("Dockerfile"),
        "# layerlock: a\nFROM old AS a\n# layerlock: b\nFROM old AS b\nFROM alpine AS unmarked\n",
    )
    .unwrap();
    fs::write(dir.path().join("Other"), "# layerlock: a\nFROM old\n").unwrap();
    let path = dir.path().join(".layerlock.toml");
    fs::write(&path, format!("dockerfiles = ['Dockerfile', 'Other']\n[groups.a]\ndependencies = ['deps']\ndockerfile = 'base'\nrepository = 'example/a'\n[groups.b]\ndependencies = ['deps']\ndockerfile = 'base'\nrepository = 'example/{}'\n", if deduplicate { "a" } else { "b" })).unwrap();
    let loaded = LoadedConfig::load(&path).unwrap();
    (dir, loaded)
}
fn prepare(loaded: &LoadedConfig, command: &Command) -> Report {
    let mut report = Report::new(command);
    plan::prepare(loaded, &[], command, &mut report).unwrap();
    report
}
#[test]
fn deduplicates_lookup_inspect_build_and_verify_and_second_run_is_noop() {
    let (dir, loaded) = fixture(true);
    let services = Services(Mutex::new(State {
        publish: true,
        ..State::default()
    }));
    let sync = Command::Sync { force: false };
    let mut report = prepare(&loaded, &sync);
    plan::resolve(&loaded, &sync, &services, &services, 2, &mut report).unwrap();
    assert_eq!(services.0.lock().unwrap().lookups, 1);
    assert_eq!(services.0.lock().unwrap().inspections, 1);
    assert!(
        report
            .groups
            .iter()
            .all(|g| g.image_action == ImageAction::BuildAndPush && !g.completed)
    );
    plan::execute(&loaded, &sync, &services, &services, &mut report).unwrap();
    assert!(report.complete);
    assert!(report.groups.iter().all(|g| g.completed));
    assert!(report.dockerfile_changes.iter().all(|c| c.completed));
    assert_eq!(services.0.lock().unwrap().builds, 1);
    assert_eq!(services.0.lock().unwrap().lookups, 2);
    let dockerfile = fs::read_to_string(dir.path().join("Dockerfile")).unwrap();
    assert!(dockerfile.contains("FROM alpine AS unmarked"));
    assert!(dockerfile.contains(" AS a\n"));
    let mut report = prepare(&loaded, &sync);
    plan::resolve(&loaded, &sync, &services, &services, 2, &mut report).unwrap();
    assert_eq!(report.work_needed, Some(false));
    assert!(report.dockerfile_changes.is_empty());
    plan::execute(&loaded, &sync, &services, &services, &mut report).unwrap();
    assert_eq!(services.0.lock().unwrap().builds, 1);
    assert_eq!(services.0.lock().unwrap().inspections, 1);
    assert_eq!(
        fs::read_to_string(dir.path().join("Dockerfile")).unwrap(),
        dockerfile
    );
}
#[test]
fn check_local_push_and_force_are_read_only_and_share_sync_plan() {
    let (dir, loaded) = fixture(true);
    let original = fs::read(dir.path().join("Dockerfile")).unwrap();
    let services = Services(Mutex::new(State {
        local: true,
        publish: true,
        ..State::default()
    }));
    let check = Command::Check { force: false };
    let mut report = prepare(&loaded, &check);
    plan::resolve(&loaded, &check, &services, &services, 8, &mut report).unwrap();
    assert!(report.complete);
    assert_eq!(report.work_needed, Some(true));
    assert!(
        report
            .groups
            .iter()
            .all(|g| g.image_action == ImageAction::PushLocal)
    );
    assert!(plan::execute(&loaded, &check, &services, &services, &mut report).is_err());
    assert_eq!(services.0.lock().unwrap().pushes, 0);
    assert_eq!(services.0.lock().unwrap().builds, 0);
    assert_eq!(fs::read(dir.path().join("Dockerfile")).unwrap(), original);

    let sync = Command::Sync { force: false };
    plan::execute(&loaded, &sync, &services, &services, &mut report).unwrap();
    assert_eq!(services.0.lock().unwrap().pushes, 1);
    let before = services.0.lock().unwrap().inspections;
    let force = Command::Check { force: true };
    let mut report = prepare(&loaded, &force);
    plan::resolve(&loaded, &force, &services, &services, 2, &mut report).unwrap();
    assert!(report.dockerfile_changes.is_empty());
    assert_eq!(report.work_needed, Some(true));
    assert!(
        report
            .groups
            .iter()
            .all(|g| g.image_action == ImageAction::ForceBuildAndPush)
    );
    assert_eq!(services.0.lock().unwrap().inspections, before);
    assert_eq!(services.0.lock().unwrap().builds, 0);
}
#[test]
fn partial_build_failure_leaves_all_dockerfiles_untouched() {
    let (dir, loaded) = fixture(false);
    let original = fs::read(dir.path().join("Dockerfile")).unwrap();
    let other = fs::read(dir.path().join("Other")).unwrap();
    let services = Services(Mutex::new(State {
        publish: true,
        fail_build_number: Some(2),
        ..State::default()
    }));
    let sync = Command::Sync { force: false };
    let mut report = prepare(&loaded, &sync);
    plan::resolve(&loaded, &sync, &services, &services, 8, &mut report).unwrap();
    assert!(plan::execute(&loaded, &sync, &services, &services, &mut report).is_err());
    assert!(!report.complete);
    assert!(report.groups[0].completed);
    assert!(!report.groups[1].completed);
    assert!(report.dockerfile_changes.iter().all(|c| !c.completed));
    assert_eq!(fs::read(dir.path().join("Dockerfile")).unwrap(), original);
    assert_eq!(fs::read(dir.path().join("Other")).unwrap(), other);
}
#[test]
fn publication_verification_failure_prevents_edits() {
    let (dir, loaded) = fixture(true);
    let services = Services(Mutex::new(State::default()));
    let sync = Command::Sync { force: false };
    let mut report = prepare(&loaded, &sync);
    plan::resolve(&loaded, &sync, &services, &services, 8, &mut report).unwrap();
    let error = plan::execute(&loaded, &sync, &services, &services, &mut report).unwrap_err();
    assert!(error.to_string().contains("verification failed"));
    assert!(!report.complete);
    assert!(!report.groups[0].completed);
    assert!(
        fs::read_to_string(dir.path().join("Other"))
            .unwrap()
            .contains("FROM old")
    );
}
#[test]
fn registry_errors_and_changed_inputs_cannot_be_executed() {
    let (dir, loaded) = fixture(true);
    let services = Services(Mutex::new(State {
        fail_lookups: true,
        publish: true,
        ..State::default()
    }));
    let sync = Command::Sync { force: false };
    let mut report = prepare(&loaded, &sync);
    assert!(plan::resolve(&loaded, &sync, &services, &services, 2, &mut report).is_err());
    assert!(!report.complete);
    assert!(plan::execute(&loaded, &sync, &services, &services, &mut report).is_err());
    services.0.lock().unwrap().fail_lookups = false;
    let mut report = prepare(&loaded, &sync);
    plan::resolve(&loaded, &sync, &services, &services, 2, &mut report).unwrap();
    fs::write(dir.path().join("deps"), "changed").unwrap();
    assert!(
        plan::execute(&loaded, &sync, &services, &services, &mut report)
            .unwrap_err()
            .to_string()
            .contains("inputs changed")
    );
    assert_eq!(services.0.lock().unwrap().builds, 0);
}
#[test]
fn build_only_and_selected_sync_leave_other_stages_alone() {
    let (dir, loaded) = fixture(false);
    let services = Services(Mutex::new(State {
        publish: true,
        ..State::default()
    }));
    let build = Command::Build { force: true };
    let mut report = prepare(&loaded, &build);
    plan::resolve(&loaded, &build, &services, &services, 8, &mut report).unwrap();
    plan::execute(&loaded, &build, &services, &services, &mut report).unwrap();
    assert!(report.dockerfile_changes.is_empty());
    assert!(
        fs::read_to_string(dir.path().join("Other"))
            .unwrap()
            .contains("FROM old")
    );
    let sync = Command::Sync { force: false };
    let mut report = Report::new(&sync);
    plan::prepare(&loaded, &["a".into()], &sync, &mut report).unwrap();
    plan::resolve(&loaded, &sync, &services, &services, 2, &mut report).unwrap();
    plan::execute(&loaded, &sync, &services, &services, &mut report).unwrap();
    assert!(
        fs::read_to_string(dir.path().join("Dockerfile"))
            .unwrap()
            .contains("# layerlock: b\nFROM old AS b")
    );
}
#[test]
fn local_tag_moving_after_plan_is_an_error_not_a_push() {
    let (_dir, loaded) = fixture(true);
    let services = Services(Mutex::new(State {
        local: true,
        publish: true,
        ..State::default()
    }));
    let build = Command::Build { force: false };
    let mut report = prepare(&loaded, &build);
    plan::resolve(&loaded, &build, &services, &services, 2, &mut report).unwrap();
    services.0.lock().unwrap().local = false;
    assert!(
        plan::execute(&loaded, &build, &services, &services, &mut report)
            .unwrap_err()
            .to_string()
            .contains("local image changed")
    );
    assert_eq!(services.0.lock().unwrap().pushes, 0);
}
