use anyhow::{Result, bail, ensure};
use serde::Serialize;
use similar::TextDiff;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Serialize)]
pub struct DockerfileChange {
    pub completed: bool,
    pub path: String,
    pub diff: String,
    #[serde(skip)]
    pub original: String,
    #[serde(skip)]
    pub updated: String,
}

/// Validate every marker, including unselected groups; replace only selected references.
pub fn rewrite(
    path: &str,
    source: &str,
    known: &BTreeSet<String>,
    references: &BTreeMap<String, String>,
) -> Result<Option<DockerfileChange>> {
    let lines: Vec<_> = source.split_inclusive('\n').collect();
    let mut output = String::with_capacity(source.len());
    let mut pending: Option<&str> = None;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(group) = pending.take() {
            let (start, end) =
                image_span(line).map_err(|err| anyhow::anyhow!("{path}:{}: {err}", index + 1))?;
            if let Some(reference) = references.get(group) {
                output.push_str(&line[..start]);
                output.push_str(reference);
                output.push_str(&line[end..]);
            } else {
                output.push_str(line);
            }
            continue;
        }
        if let Some(comment) = trimmed.strip_prefix('#') {
            let comment = comment.trim_start();
            if let Some(group) = comment.strip_prefix("layerlock:") {
                let group = group.trim();
                ensure!(
                    !group.is_empty() && known.contains(group),
                    "{path}:{}: unknown or empty layerlock group {group:?}",
                    index + 1
                );
                pending = Some(group);
            }
        }
        output.push_str(line);
    }
    ensure!(
        pending.is_none(),
        "{path}: marker has no following FROM line"
    );
    if output == source {
        return Ok(None);
    }
    let diff = TextDiff::from_lines(source, &output)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    Ok(Some(DockerfileChange {
        completed: false,
        path: path.into(),
        diff,
        original: source.into(),
        updated: output,
    }))
}

fn image_span(line: &str) -> Result<(usize, usize)> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, ch) in line.char_indices() {
        if ch.is_whitespace() {
            if let Some(begin) = start.take() {
                tokens.push((begin, index));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(begin) = start {
        tokens.push((begin, line.len()));
    }
    let words: Vec<_> = tokens
        .iter()
        .map(|&(start, end)| &line[start..end])
        .collect();
    ensure!(
        words
            .first()
            .is_some_and(|s| s.eq_ignore_ascii_case("FROM")),
        "marker must be immediately followed by FROM"
    );
    ensure!(
        !line.trim_end().ends_with('\\'),
        "continued managed FROM lines are not supported; use a single line"
    );
    let mut image = 1;
    if words
        .get(image)
        .is_some_and(|s| s.starts_with("--platform="))
    {
        ensure!(
            words[image].len() > "--platform=".len(),
            "empty FROM platform"
        );
        image += 1;
    }
    let Some(value) = words.get(image) else {
        bail!("FROM is missing an image");
    };
    ensure!(
        !value.starts_with('-') && !value.starts_with('#'),
        "invalid FROM image"
    );
    let remainder = &words[image + 1..];
    ensure!(
        remainder.is_empty()
            || (remainder.len() == 2
                && remainder[0].eq_ignore_ascii_case("AS")
                && !remainder[1].starts_with('#')),
        "invalid managed FROM syntax"
    );
    Ok(tokens[image])
}

#[cfg(test)]
mod tests {
    use super::*;
    fn known() -> BTreeSet<String> {
        ["base".into(), "other".into()].into()
    }
    fn references() -> BTreeMap<String, String> {
        [("base".into(), "registry/base:sha256-new".into())].into()
    }
    #[test]
    fn preserves_platform_alias_line_endings_and_unmarked_lines() {
        let source = "# heading\r\nFROM alpine AS untouched\r\n# layerlock: base\r\n  fRoM --platform=$BUILDPLATFORM old:tag  AS builder\r\nRUN true\r\n# layerlock: base\r\nFROM old:tag";
        let change = rewrite("Dockerfile", source, &known(), &references())
            .unwrap()
            .unwrap();
        assert_eq!(
            change.updated,
            source.replace("old:tag", "registry/base:sha256-new")
        );
        assert!(change.diff.contains("--- a/Dockerfile"));
        assert!(
            rewrite("Dockerfile", &change.updated, &known(), &references())
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn validates_all_markers_and_preserves_unselected_groups() {
        assert!(
            rewrite(
                "D",
                "# layerlock: other\nFROM old\n",
                &known(),
                &references()
            )
            .unwrap()
            .is_none()
        );
        for source in [
            "# layerlock: missing\nFROM a\n",
            "# layerlock:\nFROM a\n",
            "# layerlock: base\n",
            "# layerlock: base\n\nFROM a\n",
            "# layerlock: other\nRUN true\n",
            "# layerlock: base\nFROM a \\\n AS builder\n",
            "# layerlock: base\nFROM a # comment\n",
        ] {
            assert!(
                rewrite("D", source, &known(), &references()).is_err(),
                "{source}"
            );
        }
    }
}
