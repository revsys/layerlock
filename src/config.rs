use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub dockerfiles: Vec<String>,
    pub groups: BTreeMap<String, Group>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Group {
    pub dependencies: Vec<String>,
    pub dockerfile: String,
    pub repository: String,
    #[serde(default = "default_context")]
    pub context: String,
    #[serde(default)]
    pub build_args: BTreeMap<String, String>,
    pub target: Option<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
}
fn default_context() -> String {
    ".".into()
}

/// Only these effective settings contribute to the hash. Publication destinations do not.
#[derive(Debug, Serialize)]
pub struct BuildSettings<'a> {
    pub context: &'a str,
    pub build_args: &'a BTreeMap<String, String>,
    pub target: &'a Option<String>,
    pub platforms: &'a [String],
}

pub struct LoadedConfig {
    pub root: PathBuf,
    pub config: Config,
}
impl LoadedConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        // TOML errors include source excerpts and invalid values, which can expose
        // secret build arguments. Report location, not the parser's raw payload.
        let mut config: Config = toml::from_str(&text).map_err(|error: toml::de::Error| {
            let position = error
                .span()
                .map(|span| {
                    let prefix = &text.as_bytes()[..span.start.min(text.len())];
                    let line = prefix.iter().filter(|&&byte| byte == b'\n').count() + 1;
                    let column = prefix
                        .iter()
                        .rposition(|&byte| byte == b'\n')
                        .map(|index| prefix.len() - index)
                        .unwrap_or(prefix.len() + 1);
                    format!(" at line {line}, column {column}")
                })
                .unwrap_or_default();
            anyhow::anyhow!("invalid Layerlock TOML configuration/schema{position}")
        })?;
        ensure!(
            !config.groups.is_empty(),
            "config must define at least one group"
        );
        for (name, group) in &mut config.groups {
            ensure!(
                !name.trim().is_empty() && name.trim() == name,
                "invalid group name {name:?}"
            );
            ensure!(
                !group.dependencies.is_empty(),
                "group {name}: dependencies must not be empty\n\n\
                 Add one or more files that declare installed dependencies,\n\
                 such as requirements.txt, package.json, or a lockfile.\n\n\
                 In [groups.{name}], for example:\n\
                 dependencies = [\"requirements.txt\"]\n\n\
                 Paths are relative to your Layerlock config file.\n\
                 Layerlock hashes these files to detect dependency changes."
            );
            group.dependencies = normalized_unique(&group.dependencies)?;
            group.dockerfile = normalize_path(&group.dockerfile)?;
            group.context = normalize_path(&group.context)?;
            ensure!(
                !group.repository.is_empty()
                    && !group.repository.chars().any(char::is_whitespace)
                    && !group.repository.contains('@')
                    && !group.repository.contains("://")
                    && !group.repository.rsplit('/').next().unwrap().contains(':')
                    && !group.repository.ends_with('/'),
                "group {name}: repository must be an untagged image repository"
            );
            crate::registry::ImageReference::parse(&format!("{}:validation", group.repository))
                .with_context(|| format!("group {name}: invalid repository"))?;
            if let Some(target) = &group.target {
                ensure!(
                    !target.trim().is_empty(),
                    "group {name}: target must not be empty"
                );
            }
            ensure!(
                group
                    .build_args
                    .keys()
                    .all(|k| !k.is_empty() && !k.contains('=')),
                "group {name}: invalid build argument name"
            );
            for platform in &group.platforms {
                let parts: Vec<_> = platform.split('/').collect();
                ensure!(
                    (2..=3).contains(&parts.len())
                        && parts.iter().all(|p| !p.is_empty()
                            && p.chars().all(|c| c.is_ascii_alphanumeric()
                                || c == '_'
                                || c == '-'
                                || c == '.')),
                    "group {name}: invalid platform {platform:?}"
                );
            }
            group.platforms.sort();
            group.platforms.dedup();
        }
        config.dockerfiles = normalized_unique(&config.dockerfiles)?;
        let root = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let application_paths: BTreeSet<_> = config
            .dockerfiles
            .iter()
            .filter_map(|path| fs::canonicalize(root.join(path)).ok())
            .collect();
        for group in config.groups.values() {
            // A dependency symlink to a generated Dockerfile would create a
            // self-invalidating tag on every sync. Exclude resolved aliases too.
            for input in group
                .dependencies
                .iter()
                .chain(std::iter::once(&group.dockerfile))
            {
                if let Ok(path) = fs::canonicalize(root.join(input)) {
                    ensure!(
                        !application_paths.contains(&path),
                        "application Dockerfile alias {input} cannot be a hash input"
                    );
                }
            }
            ensure!(
                root.join(&group.context).is_dir(),
                "build context {} is not a directory",
                group.context
            );
            ensure!(
                !config.dockerfiles.contains(&group.dockerfile),
                "base-image recipe {} cannot also be an application Dockerfile",
                group.dockerfile
            );
            for dependency in &group.dependencies {
                ensure!(
                    !config.dockerfiles.contains(dependency),
                    "application Dockerfile {dependency} cannot be a hash input"
                );
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
            config,
        })
    }

    pub fn selected(&self, names: &[String]) -> Result<Vec<String>> {
        if names.is_empty() {
            return Ok(self.config.groups.keys().cloned().collect());
        }
        let names: BTreeSet<_> = names.iter().cloned().collect();
        for name in &names {
            ensure!(
                self.config.groups.contains_key(name),
                "unknown group {name:?}"
            );
        }
        Ok(names.into_iter().collect())
    }
}

fn normalized_unique(paths: &[String]) -> Result<Vec<String>> {
    Ok(paths
        .iter()
        .map(|p| normalize_path(p))
        .collect::<Result<BTreeSet<_>>>()?
        .into_iter()
        .collect())
}

/// Lexical normalization, independent of checkout location; do not canonicalize symlinks.
pub fn normalize_path(path: &str) -> Result<String> {
    ensure!(!path.is_empty(), "path must not be empty");
    ensure!(
        !path.contains('\\'),
        "use forward slashes in config paths: {path:?}"
    );
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(value) => parts.push(value.to_str().context("path is not UTF-8")?),
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(
                    parts.pop().is_some(),
                    "path escapes config directory: {path:?}"
                );
            }
            _ => bail!("config paths must be relative: {path:?}"),
        }
    }
    Ok(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn rejects_aliases_of_generated_dockerfiles_as_inputs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Dockerfile"),
            "# layerlock: base\nFROM old\n",
        )
        .unwrap();
        fs::write(dir.path().join("base"), "FROM scratch\n").unwrap();
        std::os::unix::fs::symlink("Dockerfile", dir.path().join("alias")).unwrap();
        let config = dir.path().join("config.toml");
        fs::write(&config, "dockerfiles = ['Dockerfile']\n[groups.base]\ndependencies = ['alias']\ndockerfile = 'base'\nrepository = 'example/base'\n").unwrap();
        let error = LoadedConfig::load(&config).err().unwrap();
        assert!(error.to_string().contains("cannot be a hash input"));
    }

    #[test]
    fn schema_errors_do_not_echo_secret_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "groups = 'secret-build-token'\n").unwrap();
        let error = LoadedConfig::load(&path).err().unwrap();
        assert!(!format!("{error:#}").contains("secret-build-token"));
        assert!(error.to_string().contains("line 1"));
    }

    #[test]
    fn paths_are_normalized() {
        assert_eq!(normalize_path("./a/../b//c").unwrap(), "b/c");
        assert_eq!(normalize_path(".").unwrap(), ".");
        for path in ["", "../secret", "/tmp/a", "a/../../b", "a\\b"] {
            assert!(normalize_path(path).is_err());
        }
    }
}
