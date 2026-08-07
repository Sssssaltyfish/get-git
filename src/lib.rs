use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use tempfile::Builder;

mod backend;
mod config;
#[cfg(feature = "backend-git2")]
mod materialize;
mod source;

#[cfg(not(any(feature = "backend-git2", feature = "backend-system")))]
compile_error!("get-git requires at least one of backend-git2 or backend-system");

pub use backend::Backend;
pub use config::ConfigSource;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DownloadOptions {
    pub backend: Backend,
    pub config: ConfigSource,
    /// Base directory in which get-git creates an automatically removed work directory.
    pub work_dir: Option<PathBuf>,
    pub force: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadResult {
    pub repository: String,
    pub reference: String,
    pub path: String,
    pub kind: TargetKind,
    pub output: PathBuf,
    pub backend: &'static str,
}

pub fn download(
    source_url: &str,
    explicit_ref: Option<&str>,
    explicit_path: Option<&str>,
    output: Option<PathBuf>,
    options: &DownloadOptions,
) -> Result<DownloadResult> {
    let current_dir =
        std::env::current_dir().context("Failed to determine the current directory")?;
    let parsed = source::parse(source_url)?;
    let selected_backend = backend::select(options.backend)?;
    let work = create_work_dir(options.work_dir.as_deref(), &current_dir)?;
    let prepared_config = config::prepare(&options.config, &current_dir, work.path())?;
    let advertisement = if source::requires_advertisement(explicit_ref, explicit_path) {
        backend::advertise(
            selected_backend,
            &parsed.repository_url,
            &prepared_config,
            &work.path().join("advertisement"),
        )?
    } else {
        backend::Advertisement::default()
    };
    let request = source::resolve(
        parsed,
        explicit_ref,
        explicit_path,
        output,
        &advertisement,
        &current_dir,
    )?;

    if path_exists(&request.output) && !options.force {
        bail!(
            "Output path already exists: {} (use --force to replace it)",
            request.output.display()
        );
    }
    let parent = request
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .context("The output path must have a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create output directory {}", parent.display()))?;

    // The completed payload and any force-backup live beside the destination,
    // making all installation renames stay on one filesystem.
    let stage = Builder::new()
        .prefix(".get-git-stage-")
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "Failed to create a staging directory in {}",
                parent.display()
            )
        })?;
    let payload = stage.path().join("payload");
    let kind = backend::fetch_and_materialize(
        selected_backend,
        &request,
        &prepared_config,
        &work.path().join("repository"),
        &payload,
    )?;
    install_path(&payload, &request.output, options.force, stage.path())?;

    Ok(DownloadResult {
        repository: request.repository,
        reference: request.reference,
        path: request.path,
        kind,
        output: request.output,
        backend: selected_backend.name(),
    })
}

fn create_work_dir(base: Option<&Path>, current_dir: &Path) -> Result<tempfile::TempDir> {
    let mut builder = Builder::new();
    builder.prefix("get-git-");
    match base {
        Some(base) => {
            let base = if base.is_absolute() {
                base.to_path_buf()
            } else {
                current_dir.join(base)
            };
            fs::create_dir_all(&base).with_context(|| {
                format!("Failed to create work-directory base {}", base.display())
            })?;
            builder.tempdir_in(&base).with_context(|| {
                format!(
                    "Failed to create a temporary work directory in {}",
                    base.display()
                )
            })
        }
        None => builder
            .tempdir()
            .context("Failed to create a temporary work directory"),
    }
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn install_path(source: &Path, destination: &Path, force: bool, staging_root: &Path) -> Result<()> {
    if !path_exists(destination) {
        return fs::rename(source, destination).with_context(|| {
            format!(
                "Failed to install downloaded content at {}",
                destination.display()
            )
        });
    }
    if !force {
        bail!(
            "Output path already exists: {} (use --force to replace it)",
            destination.display()
        );
    }

    let backup = staging_root.join("replaced-output");
    fs::rename(destination, &backup).with_context(|| {
        format!(
            "Failed to stage the existing output {} for replacement",
            destination.display()
        )
    })?;
    match fs::rename(source, destination) {
        Ok(()) => remove_path(&backup).with_context(|| {
            format!("Downloaded content was installed, but the old output at {} could not be removed", backup.display())
        }),
        Err(install_error) => match fs::rename(&backup, destination) {
            Ok(()) => Err(install_error).with_context(|| {
                format!("Failed to replace {}; the old output was restored", destination.display())
            }),
            Err(restore_error) => bail!(
                "Failed to replace {} ({install_error}) and failed to restore its old content from {} ({restore_error})",
                destination.display(),
                backup.display()
            ),
        },
    }
}

fn remove_path(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Failed to inspect {}", path.display()))?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("Failed to remove {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn force_install_replaces_files_and_directories() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("destination");
        let staging = temp.path().join("staging");
        fs::create_dir(&staging).unwrap();
        fs::write(&destination, "old").unwrap();
        let payload = staging.join("payload");
        fs::create_dir(&payload).unwrap();
        fs::write(payload.join("new.txt"), "new").unwrap();

        install_path(&payload, &destination, true, &staging).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("new.txt")).unwrap(),
            "new"
        );
        assert!(!staging.join("replaced-output").exists());
    }

    #[test]
    fn work_directory_uses_the_selected_base_and_is_removed() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("work");
        let path = {
            let work = create_work_dir(Some(&base), temp.path()).unwrap();
            let path = work.path().to_path_buf();
            assert!(path.starts_with(&base));
            path
        };
        assert!(!path.exists());
        assert!(base.exists());
    }
}
