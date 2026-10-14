#![cfg(feature = "backend-git2")]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use git2::{Oid, Repository, Signature};

const SOURCE: &str = "https://github.com/local/fixture.git";

#[test]
fn private_http_repository_uses_helpers_for_advertisement_and_fetch() {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        process::Stdio,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        thread,
        time::Duration,
    };

    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let fixture = Fixture::new();
    let root = fixture.temp.path().to_owned();
    let repository = Repository::open_bare(root.join("source repository.git")).unwrap();
    repository
        .config()
        .unwrap()
        .set_bool("uploadpack.allowFilter", true)
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let challenges = Arc::new(AtomicUsize::new(0));
    let authenticated = Arc::new(AtomicUsize::new(0));
    let stop = stopped.clone();
    let rejected = challenges.clone();
    let accepted = authenticated.clone();
    let server = thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut authorized = false;
            let mut length = 0;
            let mut content_type = String::new();
            let mut protocol = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                let (key, value) = line.split_once(':').unwrap();
                match key.to_ascii_lowercase().as_str() {
                    "authorization" => authorized = value.trim() == "Basic Zml4dHVyZTpzZWNyZXQ=",
                    "content-length" => length = value.trim().parse().unwrap(),
                    "content-type" => content_type = value.trim().to_owned(),
                    "git-protocol" => protocol = value.trim().to_owned(),
                    _ => {}
                }
            }
            if !authorized {
                rejected.fetch_add(1, Ordering::Relaxed);
                stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                continue;
            }
            accepted.fetch_add(1, Ordering::Relaxed);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap();
            let target = parts.next().unwrap();
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let mut child = Command::new("git")
                .arg("http-backend")
                .env("GIT_PROJECT_ROOT", &root)
                .env("GIT_HTTP_EXPORT_ALL", "1")
                .env("PATH_INFO", path.replace("%20", " "))
                .env("QUERY_STRING", query)
                .env("REQUEST_METHOD", method)
                .env("CONTENT_TYPE", content_type)
                .env("CONTENT_LENGTH", length.to_string())
                .env("HTTP_GIT_PROTOCOL", protocol)
                .env("REMOTE_USER", "fixture")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&body).unwrap();
            let response = child.wait_with_output().unwrap();
            assert!(
                response.status.success(),
                "{}",
                String::from_utf8_lossy(&response.stderr)
            );
            let split = response
                .stdout
                .windows(4)
                .position(|value| value == b"\r\n\r\n")
                .unwrap();
            let headers = String::from_utf8_lossy(&response.stdout[..split]);
            let payload = &response.stdout[split + 4..];
            write!(
                stream,
                "HTTP/1.1 200 OK\r\n{headers}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )
            .unwrap();
            stream.write_all(payload).unwrap();
        }
    });

    fs::write(
        &fixture.config,
        format!(
            r#"
[url "http://{address}/source%20repository.git"]
    insteadOf = {SOURCE}
[credential]
    helper = "!f() {{ echo username=fixture; echo password=secret; }}; f"
"#
        ),
    )
    .unwrap();
    let backends: &[&str] = &["git2"];
    #[cfg(feature = "backend-system")]
    let backends = if system_git_is_supported() {
        &["git2", "system"]
    } else {
        backends
    };
    let results = backends
        .iter()
        .map(|backend| {
            let output = fixture.temp.path().join(format!("private-{backend}"));
            let result = command(fixture.temp.path())
                .env("GIT_TERMINAL_PROMPT", "0")
                .args([
                    SOURCE,
                    "--backend",
                    backend,
                    "--git-config",
                    fixture.config.to_str().unwrap(),
                    "--ref",
                    "main",
                    "--path",
                    "assets",
                    "--output",
                    output.to_str().unwrap(),
                ])
                .output()
                .unwrap();
            (result, output)
        })
        .collect::<Vec<_>>();
    stopped.store(true, Ordering::Relaxed);
    server.join().unwrap();
    for (result, output) in results {
        assert_success(&result);
        assert_eq!(read_text(&output.join("config.txt")), "main\n");
        assert_eq!(read_text(&output.join("nested/data.txt")), "nested\n");
    }
    assert!(challenges.load(Ordering::Relaxed) >= backends.len() * 2);
    assert!(authenticated.load(Ordering::Relaxed) >= backends.len() * 2);
}

#[test]
fn embedded_backend_downloads_without_an_environment_git() {
    let fixture = Fixture::new();
    let output_root = fixture.temp.path().join("output");
    let work_base = fixture.temp.path().join("selected work base");
    fs::create_dir(&output_root).unwrap();

    let feature_file = output_root.join("config.txt");
    let result = fixture.run_without_git([
        SOURCE,
        "--backend",
        "git2",
        "--git-config",
        fixture.config.to_str().unwrap(),
        "--work-dir",
        work_base.to_str().unwrap(),
        "--ref",
        "feature/slash",
        "--path",
        "assets/config.txt",
        "--output",
        feature_file.to_str().unwrap(),
    ]);
    assert_success(&result);
    assert!(String::from_utf8_lossy(&result.stdout).contains("(git2 backend)"));
    assert_eq!(read_text(&feature_file), "feature\n");
    assert!(work_base.is_dir());
    assert_eq!(fs::read_dir(&work_base).unwrap().count(), 0);

    let directory = output_root.join("assets-copy");
    let result = fixture.run_without_git([
        "https://github.com/local/fixture/tree/main/assets",
        "--backend",
        "git2",
        "--git-config",
        fixture.config.to_str().unwrap(),
        "--output",
        directory.to_str().unwrap(),
    ]);
    assert_success(&result);
    assert_eq!(read_text(&directory.join("config.txt")), "main\n");
    assert_eq!(read_text(&directory.join("nested/data.txt")), "nested\n");

    let result = fixture.run_without_git([
        SOURCE,
        "--backend",
        "git2",
        "--git-config",
        fixture.config.to_str().unwrap(),
        "--ref",
        "main",
        "--path",
        "assets/config.txt",
        "--output",
        feature_file.to_str().unwrap(),
    ]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("already exists"));
    assert_eq!(read_text(&feature_file), "feature\n");

    let result = fixture.run_without_git([
        SOURCE,
        "--backend",
        "git2",
        "--git-config",
        fixture.config.to_str().unwrap(),
        "--ref",
        "main",
        "--path",
        "assets/config.txt",
        "--output",
        feature_file.to_str().unwrap(),
        "--force",
    ]);
    assert_success(&result);
    assert_eq!(read_text(&feature_file), "main\n");

    let wrong_kind = output_root.join("wrong-kind");
    let result = fixture.run_without_git([
        "https://github.com/local/fixture/blob/main/assets",
        "--backend",
        "git2",
        "--git-config",
        fixture.config.to_str().unwrap(),
        "--output",
        wrong_kind.to_str().unwrap(),
    ]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("URL identifies a file"));
    assert!(!wrong_kind.exists());
}

#[test]
fn default_auto_backend_falls_back_when_environment_git_is_absent() {
    let fixture = Fixture::new();
    let output = fixture.temp.path().join("auto-output.txt");
    let result = fixture.run_without_git([
        SOURCE,
        "--git-config",
        fixture.config.to_str().unwrap(),
        "--ref",
        "main",
        "--path",
        "README.md",
        "--output",
        output.to_str().unwrap(),
    ]);

    assert_success(&result);
    assert_eq!(read_text(&output), "fixture\n");
    assert!(String::from_utf8_lossy(&result.stdout).contains("(git2 backend)"));
}

#[test]
fn project_config_is_an_explicit_quick_config_source() {
    let fixture = Fixture::new();
    let project = fixture.temp.path().join("project");
    fs::create_dir_all(project.join(".git")).unwrap();
    fs::copy(&fixture.config, project.join(".git/config")).unwrap();
    let output = fixture.temp.path().join("project-config-output.txt");

    let result = command(&project)
        .env("PATH", "")
        .args([
            SOURCE,
            "--backend",
            "git2",
            "--project-config",
            "--ref",
            "main",
            "--path",
            "README.md",
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_success(&result);
    assert_eq!(read_text(&output), "fixture\n");
}

#[cfg(feature = "backend-system")]
#[test]
fn explicit_system_backend_checks_version_and_runs_when_supported() {
    let fixture = Fixture::new();
    let output = fixture.temp.path().join("system-output.txt");
    let result = command(fixture.temp.path())
        .args([
            SOURCE,
            "--backend",
            "system",
            "--git-config",
            fixture.config.to_str().unwrap(),
            "--ref",
            "main",
            "--path",
            "README.md",
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    if system_git_is_supported() {
        assert_success(&result);
        assert_eq!(read_text(&output), "fixture\n");
        assert!(String::from_utf8_lossy(&result.stdout).contains("(system backend)"));
    } else {
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains("Git 2.49") || error.contains("git executable"),
            "{error}"
        );
    }
}

#[cfg(feature = "backend-system")]
#[test]
fn direct_system_revision_checks_git_once_and_skips_ls_remote() {
    if !system_git_is_supported() {
        return;
    }

    let fixture = Fixture::new();
    let output = fixture.temp.path().join("direct-revision-output.txt");
    let trace = fixture.temp.path().join("git-trace.json");
    let result = command(fixture.temp.path())
        .env("GIT_TRACE2_EVENT", &trace)
        .args([
            SOURCE,
            "--backend",
            "system",
            "--git-config",
            fixture.config.to_str().unwrap(),
            "--ref",
            "refs/heads/main",
            "--path",
            "README.md",
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_success(&result);
    assert_eq!(read_text(&output), "fixture\n");

    let trace = fs::read_to_string(trace).unwrap();
    let starts = trace
        .lines()
        .filter(|line| line.contains("\"event\":\"start\""))
        .collect::<Vec<_>>();
    assert_eq!(
        starts
            .iter()
            .filter(|line| line.contains("\"--version\""))
            .count(),
        1,
        "Git invocation trace:\n{trace}"
    );
    assert!(
        starts.iter().all(|line| !line.contains("\"ls-remote\"")),
        "Git invocation trace:\n{trace}"
    );
}

#[test]
fn help_exposes_backend_work_directory_and_config_controls() {
    let result = command(Path::new(env!("CARGO_MANIFEST_DIR")))
        .arg("--help")
        .output()
        .unwrap();
    assert_success(&result);
    let help = String::from_utf8(result.stdout).unwrap();
    for option in [
        "--backend",
        "--work-dir",
        "--temp-dir",
        "--git-config",
        "--project-config",
        "--user-config",
        "--system-config",
    ] {
        assert!(help.contains(option), "help did not contain {option}");
    }
}

#[test]
fn configuration_source_flags_are_mutually_exclusive() {
    let result = command(Path::new(env!("CARGO_MANIFEST_DIR")))
        .args([SOURCE, "--user-config", "--system-config"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("cannot be used with"), "{error}");
}

struct Fixture {
    temp: tempfile::TempDir,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let remote_path = temp.path().join("source repository.git");
        let repository = Repository::init_bare(&remote_path).unwrap();
        let main = commit(&repository, "main\n", None, "main fixture");
        let parent = repository.find_commit(main).unwrap();
        commit(&repository, "feature\n", Some(&parent), "feature fixture");
        repository.set_head("refs/heads/main").unwrap();

        let config = temp.path().join("selected.gitconfig");
        let remote = config_path(&remote_path);
        fs::write(
            &config,
            format!(
                "[url \"{}\"]\n\tinsteadOf = {SOURCE}\n[protocol \"file\"]\n\tallow = always\n",
                remote.replace('"', "\\\"")
            ),
        )
        .unwrap();
        Self { temp, config }
    }

    fn run_without_git<const N: usize>(&self, args: [&str; N]) -> Output {
        command(self.temp.path())
            .env("PATH", "")
            .args(args)
            .output()
            .unwrap()
    }
}

fn commit(
    repository: &Repository,
    config_contents: &str,
    parent: Option<&git2::Commit<'_>>,
    message: &str,
) -> Oid {
    let readme = repository.blob(b"fixture\n").unwrap();
    let config = repository.blob(config_contents.as_bytes()).unwrap();
    let nested_data = repository.blob(b"nested\n").unwrap();

    let mut nested = repository.treebuilder(None).unwrap();
    nested.insert("data.txt", nested_data, 0o100644).unwrap();
    let nested = nested.write().unwrap();
    let mut assets = repository.treebuilder(None).unwrap();
    assets.insert("config.txt", config, 0o100644).unwrap();
    assets.insert("nested", nested, 0o040000).unwrap();
    let assets = assets.write().unwrap();
    let mut root = repository.treebuilder(None).unwrap();
    root.insert("README.md", readme, 0o100644).unwrap();
    root.insert("assets", assets, 0o040000).unwrap();
    let root = root.write().unwrap();
    let root = repository.find_tree(root).unwrap();
    let signature = Signature::now("get-git tests", "tests@example.invalid").unwrap();
    let parents = parent.into_iter().collect::<Vec<_>>();
    let reference = if parent.is_some() {
        "refs/heads/feature/slash"
    } else {
        "refs/heads/main"
    };
    repository
        .commit(
            Some(reference),
            &signature,
            &signature,
            message,
            &root,
            &parents,
        )
        .unwrap()
}

fn command(current_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_get-git"));
    command.current_dir(current_dir);
    command
}

fn config_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

#[cfg(feature = "backend-system")]
fn system_git_is_supported() -> bool {
    let Ok(output) = Command::new("git").arg("--version").output() else {
        return false;
    };
    let Some(version) = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .nth(2)
        .and_then(|value| {
            let mut parts = value.split('.');
            Some((
                parts.next()?.parse::<u32>().ok()?,
                parts.next()?.parse::<u32>().ok()?,
            ))
        })
    else {
        return false;
    };
    version >= (2, 49)
}
