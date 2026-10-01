use crate::{
    atomic,
    cli::Command,
    config::LoadedConfig,
    docker::ImageBuilder,
    dockerfile::{self, DockerfileChange},
    hash,
    registry::{self, ImageRegistry},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageAction {
    AvailabilityUnknown,
    Reuse,
    PushLocal,
    BuildAndPush,
    ForceBuildAndPush,
}
impl ImageAction {
    pub fn description(self, completed: bool) -> &'static str {
        match (self, completed) {
            (Self::AvailabilityUnknown, _) => "availability unknown",
            (Self::Reuse, false) => "reuse published image",
            (Self::Reuse, true) => "reused published image",
            (Self::PushLocal, false) => "push verified local image",
            (Self::PushLocal, true) => "pushed verified local image",
            (Self::BuildAndPush, false) => "build and push",
            (Self::BuildAndPush, true) => "built and pushed",
            (Self::ForceBuildAndPush, false) => "force rebuild and push",
            (Self::ForceBuildAndPush, true) => "forced rebuild and push completed",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct GroupPlan {
    pub name: String,
    pub expected_reference: String,
    pub image_action: ImageAction,
    pub input_digest: String,
    pub completed: bool,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub command: String,
    pub selected_groups: Vec<String>,
    pub groups: Vec<GroupPlan>,
    pub dockerfile_changes: Vec<DockerfileChange>,
    /// None means availability could not be determined; never report a clean check.
    pub work_needed: Option<bool>,
    pub complete: bool,
    pub errors: Vec<String>,
}
impl Report {
    pub fn new(command: &Command) -> Self {
        Self {
            schema_version: 1,
            command: command.name().into(),
            selected_groups: vec![],
            groups: vec![],
            dockerfile_changes: vec![],
            work_needed: None,
            complete: false,
            errors: vec![],
        }
    }
}

/// Read-only input preparation, shared by all commands. Registry planning follows this step.
pub fn prepare(
    loaded: &LoadedConfig,
    selection: &[String],
    command: &Command,
    report: &mut Report,
) -> Result<()> {
    report.selected_groups = loaded.selected(selection)?;
    let mut references = BTreeMap::new();
    for name in &report.selected_groups {
        let group = &loaded.config.groups[name];
        let digest =
            hash::digest(&loaded.root, group).with_context(|| format!("hashing group {name}"))?;
        let reference = format!("{}:sha256-{digest}", group.repository);
        references.insert(name.clone(), reference.clone());
        report.groups.push(GroupPlan {
            name: name.clone(),
            expected_reference: reference,
            image_action: ImageAction::AvailabilityUnknown,
            input_digest: digest,
            completed: false,
        });
    }
    if !matches!(command, Command::Build { .. }) {
        let known = loaded.config.groups.keys().cloned().collect();
        for path in &loaded.config.dockerfiles {
            atomic::validate_file(&loaded.root.join(path))?;
            let source = fs::read_to_string(loaded.root.join(path))
                .with_context(|| format!("reading application Dockerfile {path}"))?;
            if let Some(change) = dockerfile::rewrite(path, &source, &known, &references)? {
                report.dockerfile_changes.push(change);
            }
        }
    }
    if command.force() || !report.dockerfile_changes.is_empty() {
        report.work_needed = Some(true);
    }
    Ok(())
}

/// Resolve all registry lookups before any local inspection or mutation. Identical
/// expected references share one lookup, one local inspection, and one image action.
pub fn resolve(
    loaded: &LoadedConfig,
    command: &Command,
    registry: &dyn ImageRegistry,
    builder: &dyn ImageBuilder,
    concurrency: usize,
    report: &mut Report,
) -> Result<()> {
    let references: Vec<_> = report
        .groups
        .iter()
        .map(|g| g.expected_reference.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let availability = registry::lookup_all(registry, &references, concurrency);
    let mut actions = BTreeMap::new();
    let mut errors = Vec::new();
    for (reference, result) in availability {
        let planned = report
            .groups
            .iter()
            .find(|g| g.expected_reference == reference)
            .unwrap();
        let group = &loaded.config.groups[&planned.name];
        let result = result.and_then(|exists| {
            if command.force() {
                Ok(ImageAction::ForceBuildAndPush)
            } else if exists {
                Ok(ImageAction::Reuse)
            } else if builder.local_matches(&reference, &planned.input_digest, group)? {
                Ok(ImageAction::PushLocal)
            } else {
                Ok(ImageAction::BuildAndPush)
            }
        });
        match result {
            Ok(action) => {
                actions.insert(reference, action);
            }
            Err(error) => errors.push(format!("{reference}: {error:#}")),
        }
    }
    for group in &mut report.groups {
        if let Some(action) = actions.get(&group.expected_reference) {
            group.image_action = *action;
        }
    }
    if report.groups.iter().any(|g| {
        !matches!(
            g.image_action,
            ImageAction::Reuse | ImageAction::AvailabilityUnknown
        )
    }) {
        report.work_needed = Some(true);
    }
    if !errors.is_empty() {
        bail!("incomplete image plan:\n{}", errors.join("\n"));
    }
    report.work_needed = Some(
        !report.dockerfile_changes.is_empty()
            || report
                .groups
                .iter()
                .any(|g| g.image_action != ImageAction::Reuse),
    );
    report.complete = true;
    Ok(())
}

fn validate_inputs(loaded: &LoadedConfig, planned: &GroupPlan) -> Result<()> {
    ensure!(
        hash::digest(&loaded.root, &loaded.config.groups[&planned.name])? == planned.input_digest,
        "group {} inputs changed since planning; rerun Layerlock",
        planned.name
    );
    Ok(())
}

/// Sequential publication; Dockerfile writes start only after every image succeeds.
pub fn execute(
    loaded: &LoadedConfig,
    command: &Command,
    registry: &dyn ImageRegistry,
    builder: &dyn ImageBuilder,
    report: &mut Report,
) -> Result<()> {
    ensure!(
        report.complete && report.errors.is_empty(),
        "cannot execute an incomplete plan"
    );
    ensure!(
        !matches!(command, Command::Check { .. }),
        "check is read-only"
    );
    report.complete = false;
    for group in &report.groups {
        validate_inputs(loaded, group)?;
    }
    let mut published = BTreeSet::new();
    for index in 0..report.groups.len() {
        let planned = &report.groups[index];
        let reference = planned.expected_reference.clone();
        if published.contains(&reference) {
            continue;
        }
        let group = &loaded.config.groups[&planned.name];
        validate_inputs(loaded, planned)?;
        match planned.image_action {
            ImageAction::Reuse => {}
            ImageAction::PushLocal => {
                // Recheck provenance immediately before a push, as tags can move.
                ensure!(
                    builder.local_matches(&reference, &planned.input_digest, group)?,
                    "local image changed since planning: {reference}"
                );
                builder
                    .push(&reference)
                    .with_context(|| format!("publishing {reference}"))?;
            }
            ImageAction::BuildAndPush | ImageAction::ForceBuildAndPush => {
                builder
                    .build_and_push(&loaded.root, &reference, &planned.input_digest, group)
                    .with_context(|| format!("publishing {reference}"))?;
            }
            ImageAction::AvailabilityUnknown => bail!("cannot execute unknown image action"),
        }
        validate_inputs(loaded, planned)?;
        if planned.image_action != ImageAction::Reuse {
            ensure!(
                registry
                    .exists(&reference)
                    .with_context(|| format!("verifying publication of {reference}"))?,
                "publication verification failed: {reference} is still missing"
            );
        }
        published.insert(reference.clone());
        for planned in &mut report.groups {
            if planned.expected_reference == reference {
                planned.completed = true;
            }
        }
    }
    if matches!(command, Command::Sync { .. }) {
        for group in &report.groups {
            validate_inputs(loaded, group)?;
        }
        atomic::apply(&loaded.root, &mut report.dockerfile_changes)?;
    }
    report.complete = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preparation_is_read_only_and_build_ignores_application_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("deps"), "locked").unwrap();
        fs::write(dir.path().join("base"), "FROM scratch\n").unwrap();
        fs::write(
            dir.path().join("Dockerfile"),
            "# layerlock: base\nFROM old\n",
        )
        .unwrap();
        let path = dir.path().join(".layerlock.toml");
        fs::write(&path, "dockerfiles = ['Dockerfile']\n[groups.base]\ndependencies = ['deps']\ndockerfile = 'base'\nrepository = 'registry/base'\n").unwrap();
        let loaded = LoadedConfig::load(&path).unwrap();
        let check = Command::Check { force: false };
        let mut report = Report::new(&check);
        prepare(&loaded, &[], &check, &mut report).unwrap();
        assert_eq!(report.dockerfile_changes.len(), 1);
        assert_eq!(report.work_needed, Some(true));
        assert!(!report.complete);
        assert_eq!(
            fs::read_to_string(dir.path().join("Dockerfile")).unwrap(),
            "# layerlock: base\nFROM old\n"
        );
        fs::remove_file(dir.path().join("Dockerfile")).unwrap();
        let build = Command::Build { force: false };
        prepare(&loaded, &[], &build, &mut Report::new(&build)).unwrap();
    }
}
