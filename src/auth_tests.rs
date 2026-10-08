//! Real authenticated smart-HTTP tests with synthetic credentials only.
//! Each client runs in a subprocess so HOME/PATH changes never race other tests.
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use crate::{backend, config, source::ResolvedRequest, Backend, ConfigSource, TargetKind};

const CHILD: &str = "GET_GIT_AUTH_TEST_BACKEND";
const ROOT: &str = "GET_GIT_AUTH_TEST_ROOT";
const REMOTE: &str = "GET_GIT_AUTH_TEST_REMOTE";

#[test]
fn private_download_uses_default_credentials() {
    if let Ok(backend) = std::env::var(CHILD) {
        client(&backend);
        return;
    }
    let temp = tempfile::Builder::new()
        .prefix("get-git auth ")
        .tempdir()
        .unwrap();
    let root = temp.path();
    fs::write(root.join("empty.gitconfig"), "").unwrap();
    fs::create_dir(root.join("home")).unwrap();
    fs::create_dir(root.join("xdg")).unwrap();
    let repo = root.join("remote.git");
    git(root, ["init", "--bare", repo.to_str().unwrap()], None);
    let blob = text(git(
        root,
        [
            "--git-dir",
            repo.to_str().unwrap(),
            "hash-object",
            "-w",
            "--stdin",
        ],
        Some(b"private fixture\n"),
    ));
    let tree_input = format!("100644 blob {blob}\tREADME.md\n");
    let tree = text(git(
        root,
        ["--git-dir", repo.to_str().unwrap(), "mktree"],
        Some(tree_input.as_bytes()),
    ));
    let commit = text(git(
        root,
        ["--git-dir", repo.to_str().unwrap(), "commit-tree", &tree],
        Some(b"fixture\n"),
    ));
    git(
        root,
        [
            "--git-dir",
            repo.to_str().unwrap(),
            "update-ref",
            "refs/heads/main",
            &commit,
        ],
        None,
    );
    git(
        root,
        [
            "--git-dir",
            repo.to_str().unwrap(),
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ],
        None,
    );
    for key in ["uploadpack.allowFilter", "uploadpack.allowAnySHA1InWant"] {
        git(
            root,
            ["--git-dir", repo.to_str().unwrap(), "config", key, "true"],
            None,
        );
    }

    // Like the WSL GCM setup, the helper has an escaped absolute path with spaces.
    let helper = root.join("credential helper.sh");
    fs::write(&helper, "#!/bin/sh\nwhile IFS= read -r line; do [ -z \"$line\" ] && break; done\ncase \"$1\" in get) printf 'username=test\\npassword=token\\n\\n';; esac\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let helper = helper
        .to_str()
        .unwrap()
        .replace(' ', "\\ ")
        .replace('\\', "\\\\");
    fs::write(root.join("home/.gitconfig"), format!(
        "[credential]\n helper = \"{helper}\"\n useHttpPath\n[http]\n sslVerify = false\n proxy = http://127.0.0.1:1\n[core]\n hooksPath = /must-not-inherit\n[url \"http://127.0.0.1:1/\"]\n insteadOf = http://127.0.0.1:\n"
    )).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let remote = format!("http://{}/remote.git", listener.local_addr().unwrap());
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopped);
    let server_root = root.to_owned();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => serve(stream, &server_root),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("HTTP fixture failed: {error}"),
            }
        }
    });

    let mut modes = Vec::new();
    #[cfg(feature = "backend-git2")]
    modes.extend(["git2", "git2-no-git"]);
    #[cfg(feature = "backend-system")]
    if backend::select(Backend::System).is_ok() {
        modes.push("system");
    }
    let mut results = Vec::new();
    for (mode, source) in modes
        .into_iter()
        .flat_map(|mode| ["user", "system", "global-override"].map(|source| (mode, source)))
    {
        let label = format!("{mode}-{source}");
        let work = root.join(&label);
        fs::create_dir(&work).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "auth_tests::private_download_uses_default_credentials",
                "--nocapture",
            ])
            .env(CHILD, mode)
            .env(ROOT, root)
            .env(REMOTE, &remote)
            .env("HOME", root.join("home"))
            .env("USERPROFILE", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("xdg"))
            .env("GIT_CONFIG_SYSTEM", root.join("empty.gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG")
            .env_remove("GIT_DIR")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_WORK_TREE")
            .current_dir(&work);
        match source {
            "system" => {
                command
                    .env("GIT_CONFIG_NOSYSTEM", "0")
                    .env("GIT_CONFIG_SYSTEM", root.join("home/.gitconfig"))
                    .env("GIT_CONFIG_GLOBAL", root.join("empty.gitconfig"));
            }
            "global-override" => {
                command
                    .env("GIT_CONFIG_GLOBAL", root.join("home/.gitconfig"))
                    .env("HOME", root.join("xdg"))
                    .env("USERPROFILE", root.join("xdg"));
            }
            _ => {}
        }
        for variable in [
            "http_proxy",
            "https_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "all_proxy",
        ] {
            command.env_remove(variable);
        }
        if mode == "git2-no-git" {
            let bin = work.join("sh-only");
            fs::create_dir(&bin).unwrap();
            std::os::unix::fs::symlink("/bin/sh", bin.join("sh")).unwrap();
            command.env("PATH", bin);
        }
        results.push((label, command.output().unwrap()));
    }
    stopped.store(true, Ordering::Relaxed);
    server.join().unwrap();
    for (mode, result) in results {
        assert!(
            result.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

fn client(mode: &str) {
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let work = std::env::current_dir().unwrap();
    let remote = std::env::var(REMOTE).unwrap();
    let backend = match mode {
        #[cfg(feature = "backend-system")]
        "system" => backend::select(Backend::System).unwrap(),
        #[cfg(feature = "backend-git2")]
        "git2" | "git2-no-git" => backend::select(Backend::Git2).unwrap(),
        _ => panic!("unknown test backend"),
    };
    let prepared = config::prepare(&ConfigSource::default(), &work, &work).unwrap();
    let contents = fs::read_to_string(prepared.path()).unwrap();
    assert!(
        contents.contains("credential helper.sh") || contents.contains("credential\\\\ helper.sh")
    );
    assert!(contents.contains("sslVerify = true"));
    for excluded in ["sslVerify = false", "proxy", "hooksPath", "insteadOf"] {
        assert!(
            !contents.contains(excluded),
            "inherited unrelated setting {excluded}"
        );
    }
    let refs =
        backend::advertise(backend, &remote, &prepared, &work.join("advertisement")).unwrap();
    assert!(refs.refs.iter().any(|r| r.full_name == "refs/heads/main"));
    let request = ResolvedRequest {
        repository_url: remote.clone(),
        repository: "local/fixture".to_owned(),
        reference: "main".to_owned(),
        revision: "refs/heads/main".to_owned(),
        path: "README.md".to_owned(),
        expected_kind: Some(TargetKind::File),
        output: work.join("README.md"),
    };
    assert_eq!(
        backend::fetch_and_materialize(
            backend,
            &request,
            &prepared,
            &work.join("clone"),
            &request.output
        )
        .unwrap(),
        TargetKind::File
    );
    assert_eq!(fs::read(&request.output).unwrap(), b"private fixture\n");

    // Explicit configuration must not silently fall back to ambient credentials.
    for (index, source) in [
        ConfigSource::Bundled,
        ConfigSource::File(root.join("empty.gitconfig")),
    ]
    .into_iter()
    .enumerate()
    {
        let dir = work.join(format!("isolated-{index}"));
        fs::create_dir(&dir).unwrap();
        let prepared = config::prepare(&source, &work, &dir).unwrap();
        assert!(
            backend::advertise(backend, &remote, &prepared, &dir.join("advertisement")).is_err()
        );
    }
}

fn git(
    root: &Path,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    input: Option<&[u8]>,
) -> Output {
    let mut child = Command::new("git")
        .args(args)
        .env("GIT_CONFIG_GLOBAL", root.join("empty.gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG")
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "fixture git: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn text(output: Output) -> String {
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn serve(mut stream: TcpStream, root: &Path) {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    reader.read_line(&mut request).unwrap();
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap();
    let target = parts.next().unwrap();
    let mut authenticated = false;
    let mut length = 0;
    let mut content_type = String::new();
    let mut protocol = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.trim().split_once(':') {
            match name.to_ascii_lowercase().as_str() {
                "authorization" => authenticated = value.trim() == "Basic dGVzdDp0b2tlbg==",
                "content-length" => length = value.trim().parse::<usize>().unwrap(),
                "content-type" => content_type = value.trim().to_owned(),
                "git-protocol" => protocol = value.trim().to_owned(),
                "transfer-encoding" => assert_ne!(
                    value.trim(),
                    "chunked",
                    "small fixture requests should not be chunked"
                ),
                _ => {}
            }
        }
    }
    if !authenticated {
        stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"fixture\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        return;
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut child = Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("REQUEST_METHOD", method)
        .env("CONTENT_TYPE", content_type)
        .env("CONTENT_LENGTH", length.to_string())
        .env("REMOTE_USER", "test")
        .env("GIT_PROTOCOL", protocol)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("empty.gitconfig"))
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&body).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "http-backend failed");
    let split = output
        .stdout
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .expect("CGI headers");
    let headers = std::str::from_utf8(&output.stdout[..split]).unwrap();
    let body = &output.stdout[split + 4..];
    let status = headers
        .lines()
        .find_map(|line| line.strip_prefix("Status: "))
        .unwrap_or("200 OK");
    write!(
        stream,
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    )
    .unwrap();
    for line in headers.lines().filter(|line| !line.starts_with("Status:")) {
        write!(stream, "{line}\r\n").unwrap();
    }
    stream.write_all(b"\r\n").unwrap();
    stream.write_all(body).unwrap();
}
