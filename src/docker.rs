use crate::config::Group;
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::Path,
    process::{Command, Stdio},
};

pub const DIGEST_LABEL: &str = "io.layerlock.input-digest";
pub const VERSION_LABEL: &str = "io.layerlock.hash-version";
pub const PLATFORMS_LABEL: &str = "io.layerlock.platforms";

pub trait ImageBuilder {
    fn local_matches(&self, reference: &str, digest: &str, group: &Group) -> Result<bool>;
    fn push(&self, reference: &str) -> Result<()>;
    fn build_and_push(
        &self,
        root: &Path,
        reference: &str,
        digest: &str,
        group: &Group,
    ) -> Result<()>;
}
pub struct Docker;
impl ImageBuilder for Docker {
    fn local_matches(&self, reference: &str, digest: &str, group: &Group) -> Result<bool> {
        // An ordinary Docker inspect does not establish multi-platform index
        // completeness, nor the current buildx builder's implicit default platform.
        if group.platforms.len() != 1 {
            return Ok(false);
        }
        let output = Command::new("docker")
            .args(["image", "inspect", reference])
            .output()
            .map_err(|_| anyhow::anyhow!("cannot launch Docker for local image inspection"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
            if stderr.contains("no such image:") || stderr.contains("no such object:") {
                return Ok(false);
            }
            bail!(
                "Docker local image inspection failed; check daemon availability and permissions"
            );
        }
        let images: Vec<LocalImage> = serde_json::from_slice(&output.stdout)
            .map_err(|_| anyhow::anyhow!("invalid Docker image inspection response"))?;
        Ok(images.len() == 1 && verified_local(&images[0], digest, &group.platforms))
    }
    fn push(&self, reference: &str) -> Result<()> {
        run(
            Command::new("docker").args(["image", "push", reference]),
            "Docker push",
        )
    }
    fn build_and_push(
        &self,
        root: &Path,
        reference: &str,
        digest: &str,
        group: &Group,
    ) -> Result<()> {
        let mut command = Command::new("docker");
        // Keep the invocation directory so relative DOCKER_CONFIG and PATH entries
        // retain the same meaning used by registry authentication.
        command
            .args(["buildx", "build", "--push", "--file"])
            .arg(root.join(&group.dockerfile))
            .args(["--tag", reference]);
        command
            .arg("--label")
            .arg(format!("{DIGEST_LABEL}={digest}"));
        command.arg("--label").arg(format!("{VERSION_LABEL}=1"));
        command.arg("--label").arg(format!(
            "{PLATFORMS_LABEL}={}",
            serde_json::to_string(&group.platforms)?
        ));
        for (key, value) in &group.build_args {
            command.arg("--build-arg").arg(format!("{key}={value}"));
        }
        if let Some(target) = &group.target {
            command.arg("--target").arg(target);
        }
        if !group.platforms.is_empty() {
            command.arg("--platform").arg(group.platforms.join(","));
        }
        // --force intentionally adds neither --no-cache nor --pull.
        command.arg("--").arg(root.join(&group.context));
        run(&mut command, "Docker build and push")
    }
}
fn run(command: &mut Command, operation: &str) -> Result<()> {
    // Build logs must never contaminate the JSON stdout stream. Do not print the
    // command line: build argument values may be secrets.
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("cannot launch {operation}"))?;
    ensure!(status.success(), "{operation} failed ({status})");
    Ok(())
}

#[derive(Deserialize)]
struct LocalImage {
    #[serde(rename = "Os")]
    os: String,
    #[serde(rename = "Architecture")]
    architecture: String,
    #[serde(default, rename = "Variant")]
    variant: String,
    #[serde(rename = "Config")]
    config: ImageConfig,
}
#[derive(Deserialize)]
struct ImageConfig {
    #[serde(default, rename = "Labels")]
    labels: Option<BTreeMap<String, String>>,
}
fn verified_local(image: &LocalImage, digest: &str, platforms: &[String]) -> bool {
    if platforms.len() != 1 {
        return false;
    }
    let Some(labels) = &image.config.labels else {
        return false;
    };
    let platform = if image.variant.is_empty() {
        format!("{}/{}", image.os, image.architecture)
    } else {
        format!("{}/{}/{}", image.os, image.architecture, image.variant)
    };
    labels
        .get(DIGEST_LABEL)
        .is_some_and(|value| value == digest)
        && labels.get(VERSION_LABEL).is_some_and(|value| value == "1")
        && labels
            .get(PLATFORMS_LABEL)
            .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
            .is_some_and(|value| value == platforms)
        && platform == platforms[0]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_provenance_and_platform_completeness() {
        let mut image: LocalImage = serde_json::from_value(
            serde_json::json!({"Os":"linux", "Architecture":"amd64", "Config":{"Labels":{
                DIGEST_LABEL: "abc", VERSION_LABEL: "1", PLATFORMS_LABEL: "[\"linux/amd64\"]"
            }}}),
        )
        .unwrap();
        assert!(verified_local(&image, "abc", &["linux/amd64".into()]));
        assert!(!verified_local(&image, "other", &["linux/amd64".into()]));
        assert!(!verified_local(&image, "abc", &[]));
        assert!(!verified_local(&image, "abc", &["linux/arm64".into()]));
        assert!(!verified_local(
            &image,
            "abc",
            &["linux/amd64".into(), "linux/arm64".into()]
        ));
        image.config.labels = None;
        assert!(!verified_local(&image, "abc", &["linux/amd64".into()]));
    }
}
