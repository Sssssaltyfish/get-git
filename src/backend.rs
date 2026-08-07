use std::path::Path;

#[cfg(feature = "backend-system")]
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{ErrorKind, Write},
    path::PathBuf,
    process::{Command, Output, Stdio},
};

use anyhow::{bail, Context, Result};
use clap::ValueEnum;

use crate::{config::PreparedConfig, source::ResolvedRequest, TargetKind};

#[cfg(feature = "backend-system")]
const MINIMUM_SYSTEM_GIT: (u32, u32) = (2, 49);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum Backend {
    /// Prefer optimized system Git and fall back to the embedded backend.
    #[default]
    Auto,
    /// Use embedded libgit2 without invoking a git executable.
    #[cfg(feature = "backend-git2")]
    Git2,
    /// Use Git from PATH after checking its version.
    #[cfg(feature = "backend-system")]
    System,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemoteRef {
    pub full_name: String,
    pub short_name: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Advertisement {
    pub head_oid: Option<String>,
    pub refs: Vec<RemoteRef>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SelectedBackend {
    #[cfg(feature = "backend-git2")]
    Git2,
    #[cfg(feature = "backend-system")]
    System(ValidatedSystemGit),
}

/// Proof that the Git executable selected from PATH passed the version check.
///
/// Keeping this token in `SelectedBackend` makes the one validation performed
/// during backend selection sufficient for the rest of a download.
#[cfg(feature = "backend-system")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedSystemGit;

impl SelectedBackend {
    pub(crate) fn name(self) -> &'static str {
        match self {
            #[cfg(feature = "backend-git2")]
            Self::Git2 => "git2",
            #[cfg(feature = "backend-system")]
            Self::System(_) => "system",
        }
    }
}

pub(crate) fn select(requested: Backend) -> Result<SelectedBackend> {
    match requested {
        Backend::Auto => select_auto(),
        #[cfg(feature = "backend-git2")]
        Backend::Git2 => Ok(SelectedBackend::Git2),
        #[cfg(feature = "backend-system")]
        Backend::System => validate_system_git().map(SelectedBackend::System),
    }
}

fn select_auto() -> Result<SelectedBackend> {
    #[cfg(all(feature = "backend-system", feature = "backend-git2"))]
    {
        Ok(match validate_system_git() {
            Ok(git) => SelectedBackend::System(git),
            Err(_) => SelectedBackend::Git2,
        })
    }

    #[cfg(all(not(feature = "backend-system"), feature = "backend-git2"))]
    {
        Ok(SelectedBackend::Git2)
    }

    #[cfg(all(feature = "backend-system", not(feature = "backend-git2")))]
    {
        validate_system_git().map(SelectedBackend::System)
    }
}

pub(crate) fn advertise(
    backend: SelectedBackend,
    repository_url: &str,
    config: &PreparedConfig,
    _scratch: &Path,
) -> Result<Advertisement> {
    match backend {
        #[cfg(feature = "backend-git2")]
        SelectedBackend::Git2 => advertise_git2(repository_url, config, _scratch),
        #[cfg(feature = "backend-system")]
        SelectedBackend::System(git) => advertise_system(repository_url, config, git),
    }
}

pub(crate) fn fetch_and_materialize(
    backend: SelectedBackend,
    request: &ResolvedRequest,
    config: &PreparedConfig,
    repository_dir: &Path,
    destination: &Path,
) -> Result<TargetKind> {
    match backend {
        #[cfg(feature = "backend-git2")]
        SelectedBackend::Git2 => {
            fetch_and_materialize_git2(request, config, repository_dir, destination)
        }
        #[cfg(feature = "backend-system")]
        SelectedBackend::System(git) => {
            fetch_and_materialize_system(request, config, repository_dir, destination, git)
        }
    }
}

fn check_expected_kind(request: &ResolvedRequest, actual: TargetKind) -> Result<()> {
    if let Some(expected) = request.expected_kind {
        if expected != actual {
            bail!(
                "The URL identifies a {}, but '{}' is a {} at reference '{}'",
                kind_name(expected),
                request.path,
                kind_name(actual),
                request.reference
            );
        }
    }
    Ok(())
}

fn kind_name(kind: TargetKind) -> &'static str {
    match kind {
        TargetKind::File => "file",
        TargetKind::Directory => "directory",
    }
}

#[cfg(feature = "backend-git2")]
fn advertise_git2(
    repository_url: &str,
    config: &PreparedConfig,
    scratch: &Path,
) -> Result<Advertisement> {
    use git2::{Direction, ProxyOptions, Repository};

    let repository = Repository::init_bare(scratch)
        .with_context(|| format!("Failed to initialize {}", scratch.display()))?;
    let selected_config = attach_git2_config(&repository, config)?;
    let mut remote = repository
        .remote_anonymous(repository_url)
        .with_context(|| format!("Failed to configure remote {repository_url}"))?;
    let callbacks = git2_callbacks(&selected_config);
    let mut proxy = ProxyOptions::new();
    proxy.auto();
    let connection = remote
        .connect_auth(Direction::Fetch, Some(callbacks), Some(proxy))
        .with_context(|| format!("Failed to connect to {repository_url}"))?;

    let mut advertisement = Advertisement::default();
    for head in connection
        .list()
        .with_context(|| format!("Failed to list references from {repository_url}"))?
    {
        let name = head.name();
        if name == "HEAD" {
            advertisement.head_oid = Some(head.oid().to_string());
            continue;
        }
        if name.ends_with("^{}") {
            continue;
        }
        if let Some(short_name) = name
            .strip_prefix("refs/heads/")
            .or_else(|| name.strip_prefix("refs/tags/"))
        {
            advertisement.refs.push(RemoteRef {
                full_name: name.to_owned(),
                short_name: short_name.to_owned(),
            });
        }
    }
    Ok(advertisement)
}

#[cfg(feature = "backend-git2")]
fn fetch_and_materialize_git2(
    request: &ResolvedRequest,
    config: &PreparedConfig,
    repository_dir: &Path,
    destination: &Path,
) -> Result<TargetKind> {
    use git2::{Oid, Repository};
    let repository = Repository::init_bare(repository_dir)
        .with_context(|| format!("Failed to initialize {}", repository_dir.display()))?;
    let selected_config = attach_git2_config(&repository, config)?;
    let mut remote = repository
        .remote_anonymous(&request.repository_url)
        .with_context(|| format!("Failed to configure remote {}", request.repository_url))?;

    let refspec = if request.revision.starts_with("refs/") {
        format!("+{}:refs/get-git/source", request.revision)
    } else {
        request.revision.clone()
    };
    let shallow_result = {
        let mut fetch_options = git2_fetch_options(&selected_config, true);
        remote.fetch(&[&refspec], Some(&mut fetch_options), None)
    };
    if let Err(error) = shallow_result {
        if error.message().contains("shallow fetch is not supported") {
            let mut fetch_options = git2_fetch_options(&selected_config, false);
            remote
                .fetch(&[&refspec], Some(&mut fetch_options), None)
                .context(
                    "The transport rejected a shallow fetch and the full-fetch fallback failed",
                )?;
        } else {
            return Err(error).with_context(|| {
                format!(
                    "Failed to fetch reference '{}' from {} with the embedded backend",
                    request.reference, request.repository_url
                )
            });
        }
    }
    drop(remote);

    let commit_id = if request.revision.starts_with("refs/") {
        repository
            .find_reference("refs/get-git/source")
            .context("The embedded fetch did not create its source reference")?
            .peel_to_commit()
            .context("The fetched reference does not resolve to a commit")?
            .id()
    } else {
        let oid = Oid::from_str(&request.revision)
            .context("The embedded backend currently requires a SHA-1 object ID")?;
        repository
            .find_object(oid, None)
            .context("The requested object ID was not returned by the remote")?
            .peel_to_commit()
            .context("The requested object does not resolve to a commit")?
            .id()
    };

    let target =
        crate::materialize::resolve(&repository, commit_id, &request.path).with_context(|| {
            format!(
                "Path '{}' does not exist at reference '{}'",
                request.path, request.reference
            )
        })?;
    check_expected_kind(request, target.kind)?;
    crate::materialize::write(&repository, &target, destination)
        .with_context(|| format!("Failed to materialize '{}'", request.path))?;
    Ok(target.kind)
}

#[cfg(feature = "backend-git2")]
fn git2_fetch_options<'config>(
    config: &'config git2::Config,
    shallow: bool,
) -> git2::FetchOptions<'config> {
    use git2::{AutotagOption, FetchOptions, ProxyOptions};

    let mut proxy = ProxyOptions::new();
    proxy.auto();
    let mut options = FetchOptions::new();
    options
        .remote_callbacks(git2_callbacks(config))
        .proxy_options(proxy)
        .download_tags(AutotagOption::None);
    if shallow {
        options.depth(1);
    }
    options
}

#[cfg(feature = "backend-git2")]
fn attach_git2_config(
    repository: &git2::Repository,
    config: &PreparedConfig,
) -> Result<git2::Config> {
    let selected = git2::Config::open(config.path()).with_context(|| {
        format!(
            "Failed to open Git configuration {}",
            config.path().display()
        )
    })?;
    repository
        .set_config(&selected)
        .context("Failed to attach the selected Git configuration")?;
    Ok(selected)
}

#[cfg(feature = "backend-git2")]
fn git2_callbacks(config: &git2::Config) -> git2::RemoteCallbacks<'_> {
    use git2::{Cred, CredentialType, Error, RemoteCallbacks};

    let mut callbacks = RemoteCallbacks::new();
    let mut helper_attempted = false;
    let mut ssh_agent_attempted = false;
    callbacks.credentials(move |url, username, allowed| {
        if allowed.contains(CredentialType::USERNAME) {
            return Cred::username(username.unwrap_or("git"));
        }
        if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) && !helper_attempted {
            helper_attempted = true;
            if let Ok(credential) = Cred::credential_helper(config, url, username) {
                return Ok(credential);
            }
        }
        if allowed.contains(CredentialType::SSH_KEY) && !ssh_agent_attempted {
            ssh_agent_attempted = true;
            return Cred::ssh_key_from_agent(username.unwrap_or("git"));
        }
        if allowed.contains(CredentialType::DEFAULT) {
            return Cred::default();
        }
        Err(Error::from_str(
            "no credential supported by the selected Git configuration was available",
        ))
    });
    callbacks
}

#[cfg(feature = "backend-system")]
fn advertise_system(
    repository_url: &str,
    config: &PreparedConfig,
    _validated: ValidatedSystemGit,
) -> Result<Advertisement> {
    let git = SystemGit::new(config.path());
    let output = git.output(
        None,
        [
            OsString::from("ls-remote"),
            OsString::from(repository_url),
            OsString::from("HEAD"),
            OsString::from("refs/heads/*"),
            OsString::from("refs/tags/*"),
        ],
    )?;
    let stdout =
        String::from_utf8(output.stdout).context("Git returned non-UTF-8 reference names")?;
    parse_ls_remote(&stdout)
}

#[cfg(feature = "backend-system")]
fn fetch_and_materialize_system(
    request: &ResolvedRequest,
    config: &PreparedConfig,
    repository_dir: &Path,
    destination: &Path,
    _validated: ValidatedSystemGit,
) -> Result<TargetKind> {
    let git = SystemGit::new(config.path());
    git.run(
        None,
        [
            OsString::from("clone"),
            OsString::from("--quiet"),
            OsString::from("--no-checkout"),
            OsString::from("--depth=1"),
            OsString::from("--no-tags"),
            OsString::from("--filter=blob:none"),
            OsString::from(format!("--revision={}", request.revision)),
            OsString::from(&request.repository_url),
            repository_dir.as_os_str().to_owned(),
        ],
    )
    .with_context(|| {
        format!(
            "Failed to clone reference '{}' from {}",
            request.reference, request.repository_url
        )
    })?;

    let object_spec = format!("HEAD:{}", request.path);
    let object_type = git
        .stdout(
            Some(repository_dir),
            [
                OsString::from("cat-file"),
                OsString::from("-t"),
                OsString::from(&object_spec),
            ],
        )
        .with_context(|| {
            format!(
                "Path '{}' does not exist at reference '{}'",
                request.path, request.reference
            )
        })?;
    let kind = match object_type.trim() {
        "blob" => TargetKind::File,
        "tree" => TargetKind::Directory,
        "commit" => bail!(
            "Path '{}' is a submodule; downloading submodule contents is not supported",
            request.path
        ),
        other => bail!(
            "Unsupported Git object type '{other}' for path '{}'",
            request.path
        ),
    };
    check_expected_kind(request, kind)?;

    if kind == TargetKind::Directory {
        prefetch_tree_blobs(&git, repository_dir, &request.path)?;
    }
    git.run(
        Some(repository_dir),
        [
            OsString::from("--literal-pathspecs"),
            OsString::from("checkout"),
            OsString::from("--quiet"),
            OsString::from("HEAD"),
            OsString::from("--"),
            OsString::from(&request.path),
        ],
    )
    .with_context(|| format!("Failed to materialize '{}'", request.path))?;

    let materialized = repository_path(repository_dir, &request.path);
    fs::symlink_metadata(&materialized).with_context(|| {
        format!(
            "Git restored '{}', but the resulting path cannot be represented on this filesystem",
            request.path
        )
    })?;
    copy_path(&materialized, destination)?;
    Ok(kind)
}

#[cfg(feature = "backend-system")]
fn prefetch_tree_blobs(git: &SystemGit, repository_dir: &Path, path: &str) -> Result<()> {
    let listing = git.stdout(
        Some(repository_dir),
        [
            OsString::from("--literal-pathspecs"),
            OsString::from("ls-tree"),
            OsString::from("-r"),
            OsString::from("--format=%(objecttype)%x09%(objectname)"),
            OsString::from("HEAD"),
            OsString::from("--"),
            OsString::from(path),
        ],
    )?;
    let blob_ids = parse_ls_tree_blob_ids(&listing)?;
    if blob_ids.is_empty() {
        return Ok(());
    }

    let mut input = Vec::new();
    for oid in blob_ids {
        input.extend_from_slice(oid.as_bytes());
        input.push(b'\n');
    }
    git.run_with_input(
        Some(repository_dir),
        [
            OsString::from("fetch"),
            OsString::from("--quiet"),
            OsString::from("--no-tags"),
            OsString::from("--stdin"),
            OsString::from("origin"),
        ],
        &input,
    )
    .with_context(|| format!("Failed to fetch file contents below '{path}'"))
}

#[cfg(feature = "backend-system")]
fn parse_ls_remote(stdout: &str) -> Result<Advertisement> {
    let mut advertisement = Advertisement::default();
    for line in stdout.lines() {
        let Some((oid, name)) = line.split_once('\t') else {
            continue;
        };
        if !is_full_object_name(oid) {
            bail!("Git returned an invalid object ID from ls-remote: '{oid}'");
        }
        if name == "HEAD" {
            advertisement.head_oid = Some(oid.to_owned());
            continue;
        }
        if name.ends_with("^{}") {
            continue;
        }
        if let Some(short_name) = name
            .strip_prefix("refs/heads/")
            .or_else(|| name.strip_prefix("refs/tags/"))
        {
            advertisement.refs.push(RemoteRef {
                full_name: name.to_owned(),
                short_name: short_name.to_owned(),
            });
        }
    }
    Ok(advertisement)
}

#[cfg(feature = "backend-system")]
fn parse_ls_tree_blob_ids(listing: &str) -> Result<Vec<String>> {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    let mut blob_ids = Vec::new();
    for line in listing.lines() {
        let (object_type, oid) = line
            .split_once('\t')
            .with_context(|| format!("Git returned a malformed ls-tree record: '{line}'"))?;
        match object_type {
            "blob" => {
                if !is_full_object_name(oid) {
                    bail!("Git returned an invalid full object ID from ls-tree: '{oid}'");
                }
                if seen.insert(oid.to_owned()) {
                    blob_ids.push(oid.to_owned());
                }
            }
            "commit" => {}
            other => bail!("Git returned unexpected ls-tree object type '{other}'"),
        }
    }
    Ok(blob_ids)
}

#[cfg(feature = "backend-system")]
fn validate_system_git() -> Result<ValidatedSystemGit> {
    let version = system_git_version()?;
    if version < MINIMUM_SYSTEM_GIT {
        bail!(
            "The system backend requires Git {}.{} or newer for clone --revision (found {}.{})",
            MINIMUM_SYSTEM_GIT.0,
            MINIMUM_SYSTEM_GIT.1,
            version.0,
            version.1
        );
    }
    Ok(ValidatedSystemGit)
}

#[cfg(feature = "backend-system")]
fn system_git_version() -> Result<(u32, u32)> {
    let output = Command::new("git")
        .arg("--version")
        .output()
        .map_err(system_git_start_error)?;
    if !output.status.success() {
        bail!("git --version failed with {}", output.status);
    }
    let stdout = String::from_utf8(output.stdout).context("Git returned a non-UTF-8 version")?;
    parse_git_version(&stdout)
        .with_context(|| format!("Could not parse the installed Git version from '{stdout}'"))
}

#[cfg(feature = "backend-system")]
struct SystemGit {
    config: PathBuf,
}

#[cfg(feature = "backend-system")]
impl SystemGit {
    fn new(config: &Path) -> Self {
        Self {
            config: config.to_path_buf(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .env("GIT_CONFIG_GLOBAL", &self.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_SYSTEM")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG");
        command
    }

    fn run<I, S>(&self, cwd: Option<&Path>, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output(cwd, args).map(|_| ())
    }

    fn stdout<I, S>(&self, cwd: Option<&Path>, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.output(cwd, args)?;
        String::from_utf8(output.stdout)
            .context("Git returned output that is not valid UTF-8")
            .map(|output| output.trim().to_owned())
    }

    fn output<I, S>(&self, cwd: Option<&Path>, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output_impl(cwd, args, None)
    }

    fn run_with_input<I, S>(&self, cwd: Option<&Path>, args: I, input: &[u8]) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output_impl(cwd, args, Some(input)).map(|_| ())
    }

    fn output_impl<I, S>(&self, cwd: Option<&Path>, args: I, input: Option<&[u8]>) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect::<Vec<_>>();
        let mut command = self.command();
        command.args(&args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = if let Some(input) = input {
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().map_err(system_git_start_error)?;
            child
                .stdin
                .take()
                .context("Failed to open Git's stdin")?
                .write_all(input)
                .context("Failed to write object IDs to Git")?;
            child
                .wait_with_output()
                .context("Failed while waiting for Git")?
        } else {
            command
                .stdin(Stdio::inherit())
                .output()
                .map_err(system_git_start_error)?
        };
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            let rendered_args = args
                .iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            if detail.trim().is_empty() {
                bail!("git {rendered_args} failed with {}", output.status);
            }
            bail!("git {rendered_args} failed: {}", detail.trim());
        }
        Ok(output)
    }
}

#[cfg(feature = "backend-system")]
fn system_git_start_error(error: std::io::Error) -> anyhow::Error {
    if error.kind() == ErrorKind::NotFound {
        anyhow::anyhow!("Git was not found in PATH")
    } else {
        anyhow::anyhow!("Failed to start Git: {error}")
    }
}

#[cfg(feature = "backend-system")]
fn parse_git_version(output: &str) -> Option<(u32, u32)> {
    output.split_whitespace().find_map(|word| {
        let mut parts = word.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        Some((major, minor))
    })
}

#[cfg(feature = "backend-system")]
fn is_full_object_name(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(feature = "backend-system")]
fn repository_path(root: &Path, repo_path: &str) -> PathBuf {
    repo_path
        .split('/')
        .fold(root.to_path_buf(), |path, component| path.join(component))
}

#[cfg(feature = "backend-system")]
fn copy_path(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("Failed to inspect {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        return copy_symlink(source, destination);
    }
    if metadata.is_dir() {
        fs::create_dir(destination)
            .with_context(|| format!("Failed to create {}", destination.display()))?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_path(&entry.path(), &destination.join(entry.file_name()))?;
        }
        fs::set_permissions(destination, metadata.permissions())?;
    } else {
        fs::copy(source, destination).with_context(|| {
            format!(
                "Failed to copy {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        fs::set_permissions(destination, metadata.permissions())?;
    }
    Ok(())
}

#[cfg(all(feature = "backend-system", unix))]
fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, destination)?;
    Ok(())
}

#[cfg(all(feature = "backend-system", windows))]
fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source)?;
    let target_is_directory = source.metadata().is_ok_and(|metadata| metadata.is_dir());
    if target_is_directory {
        std::os::windows::fs::symlink_dir(target, destination)?;
    } else {
        std::os::windows::fs::symlink_file(target, destination)?;
    }
    Ok(())
}

#[cfg(all(test, feature = "backend-system"))]
mod tests {
    use super::*;

    #[test]
    fn parses_git_versions_and_enforces_the_documented_minimum() {
        assert_eq!(
            parse_git_version("git version 2.55.0.windows.3"),
            Some((2, 55))
        );
        assert_eq!(parse_git_version("git version 3.0.1"), Some((3, 0)));
        assert!(parse_git_version("not a version").is_none());
        assert!((2, 48) < MINIMUM_SYSTEM_GIT);
        assert!((2, 49) >= MINIMUM_SYSTEM_GIT);
    }

    #[test]
    fn parses_reference_advertisements_and_ignores_peeled_tags() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let text = format!(
            "{oid}\tHEAD\n{oid}\trefs/heads/main\n{oid}\trefs/tags/v1\n{oid}\trefs/tags/v1^{{}}\n"
        );
        let parsed = parse_ls_remote(&text).unwrap();
        assert_eq!(parsed.head_oid.as_deref(), Some(oid));
        assert_eq!(parsed.refs.len(), 2);
    }

    #[test]
    fn parses_and_deduplicates_tree_blobs() {
        let first = "0123456789abcdef0123456789abcdef01234567";
        let second = "89abcdef0123456789abcdef0123456789abcdef";
        let listing = format!("blob\t{first}\ncommit\t{second}\nblob\t{first}\nblob\t{second}");
        assert_eq!(
            parse_ls_tree_blob_ids(&listing).unwrap(),
            vec![first.to_owned(), second.to_owned()]
        );
    }
}
