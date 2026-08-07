use std::{ffi::OsStr, fs, path::Path};

use anyhow::{bail, Context, Result};
use git2::{Oid, Repository, TreeEntry};

use crate::TargetKind;

const MODE_MASK: i32 = 0o170000;
const MODE_TREE: i32 = 0o040000;
const MODE_FILE: i32 = 0o100000;
const MODE_SYMLINK: i32 = 0o120000;
const MODE_GITLINK: i32 = 0o160000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Target {
    id: Oid,
    mode: i32,
    pub(crate) kind: TargetKind,
}

pub(crate) fn resolve(
    repository: &Repository,
    commit_id: Oid,
    repository_path: &str,
) -> Result<Target> {
    let commit = repository
        .find_commit(commit_id)
        .context("The fetched object is not a commit")?;
    let mut tree = commit.tree().context("The commit has no readable tree")?;
    let mut components = repository_path.split('/').peekable();

    while let Some(component) = components.next() {
        let (id, mode) = {
            let entry = tree
                .get_name(component)
                .with_context(|| format!("No tree entry named '{component}'"))?;
            (entry.id(), entry.filemode())
        };

        if components.peek().is_none() {
            let kind = kind_from_mode(mode)?;
            return Ok(Target { id, mode, kind });
        }
        if mode & MODE_MASK != MODE_TREE {
            bail!("'{component}' is not a directory");
        }
        tree = repository
            .find_tree(id)
            .with_context(|| format!("Could not read tree '{component}'"))?;
    }

    bail!("A repository-relative path is required")
}

pub(crate) fn write(repository: &Repository, target: &Target, destination: &Path) -> Result<()> {
    match target.mode & MODE_MASK {
        MODE_TREE => write_tree(repository, target.id, destination),
        MODE_FILE => write_blob(repository, target.id, target.mode, destination),
        MODE_SYMLINK => write_symlink(repository, target.id, destination),
        MODE_GITLINK => {
            bail!("The selected path is a submodule; submodule contents are not downloaded")
        }
        mode => bail!("Unsupported Git tree mode {mode:o}"),
    }
}

fn kind_from_mode(mode: i32) -> Result<TargetKind> {
    match mode & MODE_MASK {
        MODE_TREE => Ok(TargetKind::Directory),
        MODE_FILE | MODE_SYMLINK => Ok(TargetKind::File),
        MODE_GITLINK => {
            bail!("The selected path is a submodule; submodule contents are not downloaded")
        }
        value => bail!("Unsupported Git tree mode {value:o}"),
    }
}

fn write_tree(repository: &Repository, id: Oid, destination: &Path) -> Result<()> {
    fs::create_dir(destination)
        .with_context(|| format!("Failed to create directory {}", destination.display()))?;
    let tree = repository
        .find_tree(id)
        .with_context(|| format!("Could not read tree object {id}"))?;

    for entry in tree.iter() {
        if entry.filemode() & MODE_MASK == MODE_GITLINK {
            // A gitlink names an object in another repository. There is no
            // content in this object database that get-git could materialize.
            continue;
        }
        let name = safe_entry_name(&entry)?;
        let child = destination.join(name);
        match entry.filemode() & MODE_MASK {
            MODE_TREE => write_tree(repository, entry.id(), &child)?,
            MODE_FILE => write_blob(repository, entry.id(), entry.filemode(), &child)?,
            MODE_SYMLINK => write_symlink(repository, entry.id(), &child)?,
            mode => bail!("Unsupported Git tree mode {mode:o}"),
        }
    }
    Ok(())
}

fn write_blob(repository: &Repository, id: Oid, mode: i32, destination: &Path) -> Result<()> {
    let blob = repository
        .find_blob(id)
        .with_context(|| format!("Could not read blob object {id}"))?;
    fs::write(destination, blob.content())
        .with_context(|| format!("Failed to write {}", destination.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let permissions = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
        fs::set_permissions(destination, fs::Permissions::from_mode(permissions))
            .with_context(|| format!("Failed to set permissions on {}", destination.display()))?;
    }
    #[cfg(not(unix))]
    let _ = mode;

    Ok(())
}

#[cfg(unix)]
fn write_symlink(repository: &Repository, id: Oid, destination: &Path) -> Result<()> {
    use std::os::{unix::ffi::OsStrExt, unix::fs::symlink};

    let blob = repository
        .find_blob(id)
        .with_context(|| format!("Could not read symbolic-link blob {id}"))?;
    let target = Path::new(OsStr::from_bytes(blob.content()));
    symlink(target, destination)
        .with_context(|| format!("Failed to create symbolic link {}", destination.display()))
}

#[cfg(windows)]
fn write_symlink(repository: &Repository, id: Oid, destination: &Path) -> Result<()> {
    // Git for Windows defaults core.symlinks=false when link creation is not
    // available. Matching that representation avoids requiring elevation.
    write_blob(repository, id, 0o100644, destination)
}

#[cfg(not(any(unix, windows)))]
fn write_symlink(repository: &Repository, id: Oid, destination: &Path) -> Result<()> {
    write_blob(repository, id, 0o100644, destination)
}

#[cfg(unix)]
fn safe_entry_name<'entry>(entry: &'entry TreeEntry<'_>) -> Result<&'entry OsStr> {
    use std::os::unix::ffi::OsStrExt;

    let bytes = entry.name_bytes();
    if bytes == b"." || bytes == b".." || bytes.contains(&b'/') || bytes.contains(&0) {
        bail!("A Git tree entry contains an unsafe file name")
    }
    Ok(OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn safe_entry_name<'entry>(entry: &'entry TreeEntry<'_>) -> Result<&'entry OsStr> {
    let name = entry
        .name()
        .context("A Git tree entry cannot be represented as UTF-8 on this platform")?;
    if matches!(name, "." | "..") || name.contains(['/', '\\', '\0']) {
        bail!("A Git tree entry contains an unsafe file name")
    }
    Ok(OsStr::new(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;

    #[test]
    fn resolves_and_materializes_files_trees_and_links() {
        let temp = tempfile::tempdir().unwrap();
        let repository = Repository::init_bare(temp.path().join("repository")).unwrap();

        let regular = repository.blob(b"regular\n").unwrap();
        let executable = repository.blob(b"#!/bin/sh\n").unwrap();
        let link = repository.blob(b"regular.txt").unwrap();

        let mut nested = repository.treebuilder(None).unwrap();
        nested.insert("regular.txt", regular, 0o100644).unwrap();
        nested.insert("run.sh", executable, 0o100755).unwrap();
        nested.insert("regular-link", link, 0o120000).unwrap();
        let nested = nested.write().unwrap();

        let mut root = repository.treebuilder(None).unwrap();
        root.insert("assets", nested, 0o040000).unwrap();
        let root = root.write().unwrap();
        let root = repository.find_tree(root).unwrap();
        let signature = Signature::now("get-git tests", "tests@example.invalid").unwrap();
        let commit = repository
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "fixture",
                &root,
                &[],
            )
            .unwrap();

        let target = resolve(&repository, commit, "assets").unwrap();
        assert_eq!(target.kind, TargetKind::Directory);
        let output = temp.path().join("output");
        write(&repository, &target, &output).unwrap();
        assert_eq!(fs::read(output.join("regular.txt")).unwrap(), b"regular\n");
        assert_eq!(fs::read(output.join("run.sh")).unwrap(), b"#!/bin/sh\n");

        #[cfg(unix)]
        assert_eq!(
            fs::read_link(output.join("regular-link")).unwrap(),
            Path::new("regular.txt")
        );
        #[cfg(windows)]
        assert_eq!(
            fs::read(output.join("regular-link")).unwrap(),
            b"regular.txt"
        );
    }
}
