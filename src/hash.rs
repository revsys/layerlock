use crate::config::{BuildSettings, Group};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

// Format v1: domain prefix, then fields with u64 big-endian byte lengths.
// Each record is (type, normalized path, raw content). Settings use canonical JSON.
fn field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}
fn record(hasher: &mut Sha256, kind: &str, path: &str, content: &[u8]) {
    field(hasher, kind.as_bytes());
    field(hasher, path.as_bytes());
    field(hasher, content);
}

pub fn digest(root: &Path, group: &Group) -> Result<String> {
    let mut hasher = Sha256::new();
    field(&mut hasher, b"layerlock:inputs:v1");
    for path in &group.dependencies {
        let bytes =
            fs::read(root.join(path)).with_context(|| format!("reading dependency {path}"))?;
        record(&mut hasher, "dependency", path, &bytes);
    }
    let recipe = fs::read(root.join(&group.dockerfile))
        .with_context(|| format!("reading base Dockerfile {}", group.dockerfile))?;
    record(&mut hasher, "dockerfile", &group.dockerfile, &recipe);
    let settings = BuildSettings {
        context: &group.context,
        build_args: &group.build_args,
        target: &group.target,
        platforms: &group.platforms,
    };
    record(&mut hasher, "settings", "", &serde_json::to_vec(&settings)?);
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LoadedConfig;
    fn fixture(dir: &Path) -> LoadedConfig {
        fs::write(dir.join("a"), "ab").unwrap();
        fs::write(dir.join("b"), "c").unwrap();
        fs::write(dir.join("base"), "FROM scratch\n").unwrap();
        fs::write(dir.join("config.toml"), "[groups.base]\ndependencies = ['b', './a']\ndockerfile = 'base'\nrepository = 'example/base'\n").unwrap();
        LoadedConfig::load(&dir.join("config.toml")).unwrap()
    }
    #[test]
    fn canonical_defaults_paths_arguments_and_platforms() {
        let dir = tempfile::tempdir().unwrap();
        let original = fixture(dir.path());
        let expected = digest(&original.root, &original.config.groups["base"]).unwrap();
        let config = dir.path().join("config.toml");
        fs::write(&config, "[groups.base]\ndependencies = ['./a', 'b', 'a']\ndockerfile = './base'\ncontext = './'\nrepository = 'example/base'\nbuild-args = {}\nplatforms = []\n").unwrap();
        let loaded = LoadedConfig::load(&config).unwrap();
        assert_eq!(
            expected,
            digest(&loaded.root, &loaded.config.groups["base"]).unwrap()
        );
        let first = "[groups.base]\ndependencies = ['a', 'b']\ndockerfile = 'base'\nrepository = 'example/base'\nplatforms = ['linux/arm64', 'linux/amd64']\nbuild-args = { Z = 'z', A = 'a' }\n";
        fs::write(&config, first).unwrap();
        let loaded = LoadedConfig::load(&config).unwrap();
        let with_settings = digest(&loaded.root, &loaded.config.groups["base"]).unwrap();
        assert_ne!(expected, with_settings);
        fs::write(
            &config,
            first
                .replace(
                    "['linux/arm64', 'linux/amd64']",
                    "['linux/amd64', 'linux/arm64', 'linux/amd64']",
                )
                .replace("{ Z = 'z', A = 'a' }", "{ A = 'a', Z = 'z' }"),
        )
        .unwrap();
        let mut loaded = LoadedConfig::load(&config).unwrap();
        assert_eq!(
            with_settings,
            digest(&loaded.root, &loaded.config.groups["base"]).unwrap()
        );
        let group = loaded.config.groups.get_mut("base").unwrap();
        group.target = Some("deps".into());
        assert_ne!(with_settings, digest(&loaded.root, group).unwrap());
        let before = digest(&loaded.root, group).unwrap();
        group.context = "nested".into();
        assert_ne!(before, digest(&loaded.root, group).unwrap());
    }

    #[test]
    fn stable_across_checkouts_and_sensitive_to_inputs() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let mut ca = fixture(a.path());
        let cb = fixture(b.path());
        let initial = digest(&ca.root, &ca.config.groups["base"]).unwrap();
        assert_eq!(initial.len(), 64);
        assert_eq!(
            initial,
            digest(&cb.root, &cb.config.groups["base"]).unwrap()
        );
        // Same concatenated content, different boundaries must not collide.
        fs::write(a.path().join("a"), "a").unwrap();
        fs::write(a.path().join("b"), "bc").unwrap();
        assert_ne!(
            initial,
            digest(&ca.root, &ca.config.groups["base"]).unwrap()
        );
        let group = ca.config.groups.get_mut("base").unwrap();
        let before = digest(&ca.root, group).unwrap();
        group.build_args.insert("VERSION".into(), "2".into());
        assert_ne!(before, digest(&ca.root, group).unwrap());
        let before = digest(&ca.root, group).unwrap();
        group.repository = "elsewhere/base".into();
        assert_eq!(before, digest(&ca.root, group).unwrap());
        fs::write(a.path().join("base"), "FROM alpine\n").unwrap();
        assert_ne!(before, digest(&ca.root, group).unwrap());
    }
}
