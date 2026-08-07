use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use get_git::{download, Backend, ConfigSource, DownloadOptions};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Download one file or directory from a GitHub repository",
    long_about = "Download one file or directory from a GitHub repository. The default auto backend prefers Git 2.49+ for partial-clone performance and falls back to embedded libgit2."
)]
struct Cli {
    /// A GitHub blob/tree/raw URL, or a repository URL when --path is supplied
    source: String,

    /// Exact output file or directory path (defaults to the source basename)
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// Explicit branch, tag, commit, or other fetchable Git reference
    #[arg(short = 'r', long = "ref", value_name = "REF")]
    reference: Option<String>,

    /// Explicit repository-relative file or directory path
    #[arg(short, long, value_name = "REPO_PATH")]
    path: Option<String>,

    /// Git implementation used for network and object transfer
    #[arg(long, value_enum, default_value_t = Backend::Auto)]
    backend: Backend,

    /// Base directory for an auto-removed temporary working directory
    #[arg(long, visible_alias = "temp-dir", value_name = "DIR")]
    work_dir: Option<PathBuf>,

    /// Use exactly this Git configuration file
    #[arg(long, value_name = "PATH", group = "config-source")]
    git_config: Option<PathBuf>,

    /// Use the current project's local Git configuration
    #[arg(long, group = "config-source")]
    project_config: bool,

    /// Use the standard per-user Git configuration files
    #[arg(long, group = "config-source")]
    user_config: bool,

    /// Use the standard system-wide Git configuration file
    #[arg(long, group = "config-source")]
    system_config: bool,

    /// Replace an existing output path after the download succeeds
    #[arg(short, long)]
    force: bool,

    /// Suppress the success message
    #[arg(short, long)]
    quiet: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = match cli.git_config {
        Some(path) => ConfigSource::File(path),
        None if cli.project_config => ConfigSource::Project,
        None if cli.user_config => ConfigSource::User,
        None if cli.system_config => ConfigSource::System,
        None => ConfigSource::Bundled,
    };
    let options = DownloadOptions {
        backend: cli.backend,
        config,
        work_dir: cli.work_dir,
        force: cli.force,
    };
    let result = download(
        &cli.source,
        cli.reference.as_deref(),
        cli.path.as_deref(),
        cli.output,
        &options,
    )?;

    if !cli.quiet {
        println!(
            "Downloaded {}@{}:{} -> {} ({} backend)",
            result.repository,
            result.reference,
            result.path,
            result.output.display(),
            result.backend
        );
    }

    Ok(())
}
