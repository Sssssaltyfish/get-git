use std::{io::Write, path::Path, process::Stdio};

use git2::{Config, Cred, Error};

pub(crate) fn fill(
    config: &Config,
    config_path: &Path,
    url: &str,
    username: Option<&str>,
) -> Result<Cred, Error> {
    // Credential protocol records must not be injectable through a URL or username.
    if [Some(url), username]
        .into_iter()
        .flatten()
        .any(|value| value.contains(['\n', '\r', '\0']))
    {
        return Err(Error::from_str("Invalid credential request"));
    }
    let mut command = crate::config::isolated_git_command(config_path);
    command
        .args(["credential", "fill"])
        .current_dir(config_path.parent().expect("prepared config has a parent"))
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        // Keep standalone helpers working in an embedded-only installation.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Cred::credential_helper(config, url, username);
        }
        Err(_) => return Err(Error::from_str("Failed to start Git credential lookup")),
    };
    let mut request = format!("url={url}\n");
    if let Some(username) = username {
        request.push_str(&format!("username={username}\n"));
    }
    request.push('\n');
    let written = child.stdin.take().unwrap().write_all(request.as_bytes());
    let output = child
        .wait_with_output()
        .map_err(|_| Error::from_str("Failed to wait for Git credential lookup"))?;
    // Never include helper output in errors: it can contain a password or token.
    if written.is_err() || !output.status.success() {
        return Err(Error::from_str(
            "Git credential lookup failed; check the selected credential helper and login",
        ));
    }
    let (username, password) = parse_response(&output.stdout)?;
    Cred::userpass_plaintext(username, password)
}

fn parse_response(bytes: &[u8]) -> Result<(&str, &str), Error> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::from_str("Git returned non-UTF-8 credentials"))?;
    let mut username = None;
    let mut password = None;
    for line in text.lines().take_while(|line| !line.is_empty()) {
        if let Some(value) = line.strip_prefix("username=") {
            username = Some(value);
        } else if let Some(value) = line.strip_prefix("password=") {
            password = Some(value);
        }
    }
    username
        .zip(password)
        .ok_or_else(|| Error::from_str("Git did not return a username and password"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_preserves_equals_and_stops_at_record_boundary() {
        assert_eq!(
            parse_response(b"username=user\r\npassword=a=b=c\r\n\r\npassword=wrong\n").unwrap(),
            ("user", "a=b=c")
        );
        assert!(!parse_response(b"password=secret\n")
            .unwrap_err()
            .message()
            .contains("secret"));
    }

    #[test]
    fn git_honors_helper_chains_resets_and_scoped_configuration() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config");
        std::fs::write(&path, r#"
[credential]
    helper = "!f() { echo password=wrong; echo quit=true; }; f"
    helper =
    helper = "!f() { echo username=fixture; }; f"
[credential "https://example.invalid"]
    useHttpPath = true
    namespace = fixture-namespace
    helper = "!f() { test \"$(git config credential.https://example.invalid.namespace)\" = fixture-namespace || exit 1; while IFS= read -r line; do test \"$line\" = path=owner/private.git && printf 'password=fixture\\n'; done; }; f"
"#).unwrap();
        let config = Config::open(&path).unwrap();
        fill(
            &config,
            &path,
            "https://example.invalid/owner/private.git",
            None,
        )
        .unwrap();
        assert!(fill(
            &config,
            &path,
            "https://example.invalid/\npassword=injected",
            None
        )
        .is_err());
    }
}
