//! Stage every edit before replacement. Individual renames are atomic, not the batch.
use crate::dockerfile::DockerfileChange;
use anyhow::{Context, Result, ensure};
use std::{fs, io::Write, path::Path};
use tempfile::NamedTempFile;

pub fn validate_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading {} metadata", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "managed Dockerfile {} must be a regular file, not a symlink",
        path.display()
    );
    Ok(())
}
fn unchanged(root: &Path, change: &DockerfileChange) -> Result<()> {
    let path = root.join(&change.path);
    validate_file(&path)?;
    let source = fs::read_to_string(&path).with_context(|| format!("rereading {}", change.path))?;
    ensure!(
        source == change.original,
        "Dockerfile {} changed since planning; refusing to overwrite it",
        change.path
    );
    Ok(())
}
pub fn apply(root: &Path, changes: &mut [DockerfileChange]) -> Result<()> {
    apply_with(root, changes, |temporary, path| {
        temporary
            .persist(path)
            .map_err(|error| error.error)
            .context("atomic replacement failed")?;
        Ok(())
    })
}
fn apply_with(
    root: &Path,
    changes: &mut [DockerfileChange],
    mut replace: impl FnMut(NamedTempFile, &Path) -> Result<()>,
) -> Result<()> {
    let mut staged = Vec::new();
    for change in changes.iter() {
        unchanged(root, change)?;
        let path = root.join(&change.path);
        let mut temporary = NamedTempFile::new_in(
            path.parent()
                .context("Dockerfile has no parent directory")?,
        )
        .with_context(|| format!("staging {}", change.path))?;
        temporary
            .write_all(change.updated.as_bytes())
            .with_context(|| format!("staging {}", change.path))?;
        temporary
            .as_file()
            .set_permissions(fs::metadata(&path)?.permissions())?;
        temporary
            .as_file()
            .sync_all()
            .with_context(|| format!("flushing {}", change.path))?;
        staged.push(temporary);
    }
    for (index, (change, temporary)) in changes.iter_mut().zip(staged).enumerate() {
        let result =
            unchanged(root, change).and_then(|()| replace(temporary, &root.join(&change.path)));
        result.with_context(|| format!("writing {}: {index} prior Dockerfile replacement(s) completed; batch is not a transaction", change.path))?;
        change.completed = true;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn change(path: &str) -> DockerfileChange {
        DockerfileChange {
            path: path.into(),
            diff: String::new(),
            original: "old\n".into(),
            updated: "new\n".into(),
            completed: false,
        }
    }
    #[test]
    fn stages_all_files_and_refuses_concurrent_edits() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), "old\n").unwrap();
        fs::write(dir.path().join("b"), "someone else's edit\n").unwrap();
        let mut changes = [change("a"), change("b")];
        assert!(apply(dir.path(), &mut changes).is_err());
        assert_eq!(fs::read_to_string(dir.path().join("a")).unwrap(), "old\n");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }
    #[test]
    fn reports_partial_replacement_and_preserves_permissions() {
        let dir = tempfile::tempdir().unwrap();
        for path in ["a", "b"] {
            fs::write(dir.path().join(path), "old\n").unwrap();
        }
        let permissions = fs::metadata(dir.path().join("a")).unwrap().permissions();
        let mut changes = [change("a"), change("b")];
        let mut calls = 0;
        let error = apply_with(dir.path(), &mut changes, |temp, path| {
            calls += 1;
            if calls == 2 {
                anyhow::bail!("injected failure");
            }
            temp.persist(path).map_err(|error| error.error)?;
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("1 prior"));
        assert!(changes[0].completed);
        assert!(!changes[1].completed);
        assert_eq!(fs::read_to_string(dir.path().join("a")).unwrap(), "new\n");
        assert_eq!(fs::read_to_string(dir.path().join("b")).unwrap(), "old\n");
        assert_eq!(
            fs::metadata(dir.path().join("a")).unwrap().permissions(),
            permissions
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}
