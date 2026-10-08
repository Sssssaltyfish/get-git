use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};

const BUNDLED_CONFIG: &str = include_str!("bundled.gitconfig");

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ConfigSource {
    /// Use bundled transport defaults and system/user credential settings.
    #[default]
    Default,
    /// Use only the small configuration compiled into the executable.
    Bundled,
    /// Use the local configuration of the project containing the current directory.
    Project,
    /// Use XDG and home-directory Git configuration files, in Git's precedence order.
    User,
    /// Use the platform's system-wide Git configuration file, when present.
    System,
    /// Use exactly one explicitly named Git configuration file.
    File(PathBuf),
}

#[derive(Debug)]
pub(crate) struct PreparedConfig {
    path: PathBuf,
}

impl PreparedConfig {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) fn prepare(
    source: &ConfigSource,
    current_dir: &Path,
    work_dir: &Path,
) -> Result<PreparedConfig> {
    let path = work_dir.join("gitconfig");
    let contents = match source {
        ConfigSource::Default => default_config(work_dir)?,
        ConfigSource::Bundled => BUNDLED_CONFIG.to_owned(),
        ConfigSource::File(selected) => {
            let selected = existing_file(absolute(selected, current_dir)?, "Git configuration")?;
            include_config(&[selected])?
        }
        ConfigSource::Project => {
            let files = project_config_files(current_dir)?;
            include_config(&files)?
        }
        ConfigSource::User => include_config(&user_config_files())?,
        ConfigSource::System => include_config(&system_config_files())?,
    };
    fs::write(&path, contents)
        .with_context(|| format!("Failed to prepare Git configuration at {}", path.display()))?;

    #[cfg(feature = "backend-git2")]
    git2::Config::open(&path)
        .with_context(|| format!("Invalid Git configuration selected via {}", path.display()))?;

    Ok(PreparedConfig { path })
}

// Authentication is independent of transport configuration: a private download
// should use the installed credential manager without also enabling ambient URL
// rewrites, checkout filters, proxies, or TLS overrides. Preserve entry order and
// empty helper values, which Git uses to reset a lower-priority helper chain.
fn default_config(work_dir: &Path) -> Result<String> {
    let mut contents = BUNDLED_CONFIG.to_owned();
    for (name, value) in credential_settings(work_dir)? {
        append_credential_setting(&mut contents, &name, &value);
    }
    Ok(contents)
}

fn append_credential_setting(contents: &mut String, name: &str, value: &str) {
    let Some(name) = name.strip_prefix("credential.") else {
        return;
    };
    if let Some((url, variable)) = name.rsplit_once('.') {
        contents.push_str(&format!(
            "\n[credential \"{}\"]\n\t{variable} = \"{}\"\n",
            escape_config_value(url),
            escape_config_value(value)
        ));
    } else {
        contents.push_str(&format!(
            "\n[credential]\n\t{name} = \"{}\"\n",
            escape_config_value(value)
        ));
    }
}

fn escape_config_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\u{8}', "\\b")
}

fn credential_settings(_work_dir: &Path) -> Result<Vec<(String, String)>> {
    #[cfg(feature = "backend-system")]
    {
        use std::process::{Command, Stdio};

        // Query only credentials and prevent repository discovery, even when
        // --work-dir is inside another checkout. Git resolves its own system
        // installation path, includes, and GIT_CONFIG_* overrides correctly.
        let output = Command::new("git")
            .args([
                "config",
                "--includes",
                "--null",
                "--get-regexp",
                "^credential\\.",
            ])
            .current_dir(_work_dir)
            .env(
                "GIT_CEILING_DIRECTORIES",
                _work_dir.parent().unwrap_or(_work_dir),
            )
            .env_remove("GIT_DIR")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_CONFIG")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .stdin(Stdio::null())
            .output();
        match output {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8(output.stdout)
                    .context("Git credential configuration is not valid UTF-8")?;
                return text
                    .split('\0')
                    .filter(|entry| !entry.is_empty())
                    .map(|entry| {
                        let (key, value) = entry.split_once('\n').unwrap_or((entry, "true"));
                        Ok((key.to_owned(), value.to_owned()))
                    })
                    .collect();
            }
            Ok(output) if output.status.code() == Some(1) => return Ok(Vec::new()),
            // Do not include configuration output: helper definitions may
            // contain secrets. Explicit isolated configuration is still usable.
            Ok(_) => bail!(
                "Failed to read Git credential configuration; check your Git configuration or use --bundled-config"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Failed to read Git credential configuration"),
        }
    }

    #[cfg(feature = "backend-git2")]
    {
        let config = credential_config_git2()?;
        let mut settings = Vec::new();
        let mut entries = config.entries(Some("^credential\\."))?;
        while let Some(entry) = entries.next() {
            let entry = entry?;
            let name = entry
                .name()
                .context("Git credential key is not valid UTF-8")?;
            let value = if entry.has_value() {
                entry
                    .value()
                    .context("Git credential value is not valid UTF-8")?
            } else {
                "true"
            };
            settings.push((name.to_owned(), value.to_owned()));
        }
        Ok(settings)
    }
    #[cfg(not(feature = "backend-git2"))]
    bail!("Git was not found in PATH")
}

#[cfg(feature = "backend-git2")]
fn credential_config_git2() -> Result<git2::Config> {
    use git2::{Config, ConfigLevel};

    // libgit2's default discovery does not honor Git's environment overrides.
    // Resolve those explicitly so embedded-only/no-Git runs use the same files.
    let mut sources = Vec::new();
    let no_system = env::var("GIT_CONFIG_NOSYSTEM").unwrap_or_default();
    let no_system = match no_system.to_ascii_lowercase().as_str() {
        "" | "false" | "no" | "off" => false,
        "true" | "yes" | "on" => true,
        value => {
            value
                .parse::<i64>()
                .context("Invalid GIT_CONFIG_NOSYSTEM")?
                != 0
        }
    };
    if !no_system {
        if let Some(path) = env::var_os("GIT_CONFIG_SYSTEM")
            .map(PathBuf::from)
            .or_else(|| Config::find_system().ok())
        {
            sources.push((ConfigLevel::System, path));
        }
    }
    if let Some(path) = env::var_os("GIT_CONFIG_GLOBAL") {
        sources.push((ConfigLevel::Global, PathBuf::from(path)));
    } else {
        if let Ok(path) = Config::find_xdg() {
            sources.push((ConfigLevel::XDG, path));
        }
        if let Ok(path) = Config::find_global() {
            sources.push((ConfigLevel::Global, path));
        }
    }
    let mut config = Config::new()?;
    for (level, path) in sources {
        if path.is_file() {
            config
                .add_file(&path, level, true)
                .context("Failed to read system/user Git credential configuration")?;
        }
    }
    Ok(config)
}

fn include_config(paths: &[PathBuf]) -> Result<String> {
    let mut contents = String::from(
        "# Generated by get-git. Only the explicitly selected sources are included.\n",
    );
    if paths.is_empty() {
        return Ok(contents);
    }
    contents.push_str("[include]\n");
    for path in paths {
        contents.push_str("\tpath = \"");
        contents.push_str(&escape_path(path)?);
        contents.push_str("\"\n");
    }
    Ok(contents)
}

fn escape_path(path: &Path) -> Result<String> {
    let value = path.to_str().with_context(|| {
        format!(
            "Git configuration path is not valid UTF-8: {}",
            path.display()
        )
    })?;
    #[cfg(windows)]
    let value = value.replace('\\', "/");
    #[cfg(not(windows))]
    let value = value.to_owned();

    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\u{8}' => escaped.push_str("\\b"),
            other => escaped.push(other),
        }
    }
    Ok(escaped)
}

fn absolute(path: &Path, current_dir: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(current_dir.join(path))
    }
}

fn existing_file(path: PathBuf, label: &str) -> Result<PathBuf> {
    if !path.is_file() {
        bail!("{label} is not a readable file: {}", path.display());
    }
    Ok(path)
}

fn project_config_files(current_dir: &Path) -> Result<Vec<PathBuf>> {
    for ancestor in current_dir.ancestors() {
        let dot_git = ancestor.join(".git");
        if dot_git.is_dir() {
            let config = dot_git.join("config");
            if config.is_file() {
                return Ok(with_optional_worktree_config(config, &dot_git));
            }
        } else if dot_git.is_file() {
            let git_dir = resolve_git_dir_file(&dot_git)?;
            let common_dir = resolve_common_dir(&git_dir)?;
            let config = common_dir.join("config");
            if config.is_file() {
                return Ok(with_optional_worktree_config(config, &git_dir));
            }
        }

        // Also support running from the root of a bare repository.
        if ancestor.join("HEAD").is_file()
            && ancestor.join("objects").is_dir()
            && ancestor.join("config").is_file()
        {
            return Ok(vec![ancestor.join("config")]);
        }
    }
    bail!("--project-config requires the current directory to be inside a Git repository")
}

fn with_optional_worktree_config(config: PathBuf, git_dir: &Path) -> Vec<PathBuf> {
    let mut files = vec![config];
    let worktree = git_dir.join("config.worktree");
    if worktree.is_file() {
        files.push(worktree);
    }
    files
}

fn resolve_git_dir_file(dot_git: &Path) -> Result<PathBuf> {
    let contents = fs::read_to_string(dot_git)
        .with_context(|| format!("Failed to read linked-worktree file {}", dot_git.display()))?;
    let value = contents
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .context("The project's .git file does not contain a gitdir pointer")?;
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        dot_git.parent().expect(".git file has a parent").join(path)
    })
}

fn resolve_common_dir(git_dir: &Path) -> Result<PathBuf> {
    let path = git_dir.join("commondir");
    if !path.is_file() {
        return Ok(git_dir.to_path_buf());
    }
    let value =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    let common = PathBuf::from(value.trim());
    Ok(if common.is_absolute() {
        common
    } else {
        git_dir.join(common)
    })
}

fn user_config_files() -> Vec<PathBuf> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let xdg = env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    [xdg.join("git/config"), home.join(".gitconfig")]
        .into_iter()
        .filter(|path| path.is_file())
        .collect()
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| {
            let drive = env::var_os("HOMEDRIVE")?;
            let path = env::var_os("HOMEPATH")?;
            Some(PathBuf::from(drive).join(path))
        })
}

fn system_config_files() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    #[cfg(windows)]
    {
        for root in [
            env::var_os("PROGRAMFILES"),
            env::var_os("ProgramFiles(x86)"),
            env::var_os("LOCALAPPDATA")
                .map(|path| PathBuf::from(path).join("Programs").into_os_string()),
        ]
        .into_iter()
        .flatten()
        {
            candidates.push(PathBuf::from(root).join("Git/etc/gitconfig"));
        }
    }
    #[cfg(not(windows))]
    {
        candidates.push(PathBuf::from("/etc/gitconfig"));
        candidates.push(PathBuf::from("/usr/local/etc/gitconfig"));
    }
    candidates
        .into_iter()
        .filter(|path| path.is_file())
        .take(1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_copy_preserves_order_scopes_resets_and_quoting() {
        let mut contents = BUNDLED_CONFIG.to_owned();
        for (name, value) in [
            ("credential.helper", "first"),
            ("credential.helper", ""),
            ("credential.helper", "/path/with\\ spaces/manager"),
            (
                "credential.https://example.invalid:8443/team.usehttppath",
                "true",
            ),
            ("credential.username", "a\"b\\c\nd\te"),
            ("http.proxy", "must-not-inherit"),
            ("core.hooksPath", "must-not-inherit"),
        ] {
            append_credential_setting(&mut contents, name, value);
        }
        assert!(!contents.contains("must-not-inherit"));
        assert!(contents.contains("[credential \"https://example.invalid:8443/team\"]"));
        assert!(contents.contains("helper = \"\""));
        #[cfg(feature = "backend-git2")]
        {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("gitconfig");
            fs::write(&path, contents).unwrap();
            let config = git2::Config::open(&path).unwrap();
            let mut helpers = Vec::new();
            config
                .multivar("credential.helper", None)
                .unwrap()
                .for_each(|entry| {
                    helpers.push(entry.value().unwrap().to_owned());
                })
                .unwrap();
            assert_eq!(helpers, ["first", "", "/path/with\\ spaces/manager"]);
            assert_eq!(
                config.get_string("credential.username").unwrap(),
                "a\"b\\c\nd\te"
            );
            assert!(config
                .get_bool("credential.https://example.invalid:8443/team.usehttppath")
                .unwrap());
        }
    }

    #[cfg(feature = "backend-system")]
    #[test]
    fn credential_discovery_does_not_read_the_work_directorys_parent_repository() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let git = temp.path().join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir(git.join("refs")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[credential]\n helper = get-git-test-must-not-inherit\n",
        )
        .unwrap();
        let work = temp.path().join("work");
        fs::create_dir(&work).unwrap();
        let settings = credential_settings(&work).unwrap();
        assert!(!settings
            .iter()
            .any(|(_, value)| value == "get-git-test-must-not-inherit"));
    }

    #[test]
    fn bundled_config_is_valid_and_portable() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = prepare(&ConfigSource::Bundled, temp.path(), temp.path()).unwrap();
        let text = fs::read_to_string(prepared.path()).unwrap();
        assert!(text.contains("sslVerify = true"));
        assert!(!text.contains("credential.helper"));
        assert!(!text.contains("proxy"));
    }

    #[cfg(feature = "backend-git2")]
    #[test]
    fn explicit_config_is_the_only_included_source() {
        let temp = tempfile::tempdir().unwrap();
        let selected = temp.path().join("selected.gitconfig");
        fs::write(&selected, "[http]\n\tuserAgent = test-agent\n").unwrap();
        let work = temp.path().join("work");
        fs::create_dir(&work).unwrap();
        let prepared = prepare(&ConfigSource::File(selected), temp.path(), &work).unwrap();
        let config = git2::Config::open(prepared.path()).unwrap();
        assert_eq!(config.get_string("http.userAgent").unwrap(), "test-agent");
        assert!(config.get_string("user.name").is_err());
    }

    #[test]
    fn discovers_project_config_without_invoking_git() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("project/src/nested");
        fs::create_dir_all(&nested).unwrap();
        let dot_git = temp.path().join("project/.git");
        fs::create_dir(&dot_git).unwrap();
        fs::write(dot_git.join("config"), "[http]\n\tsslVerify = true\n").unwrap();
        let files = project_config_files(&nested).unwrap();
        assert_eq!(files, vec![dot_git.join("config")]);
    }
}
