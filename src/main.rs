use std::{env, fs, io::ErrorKind, process::Command};

use anyhow::anyhow;
use clap::Parser;
use http::Uri;
use itertools::Itertools;
use tempdir::TempDir;

#[derive(Parser)]
struct Cli {
    pub uri: Uri,
}

macro_rules! exec {
    ( $name:tt; $dir:expr, $args:tt ) => {
        Command::new("git")
            .args($args)
            .current_dir($dir)
            .status()
            .map_err(|e| anyhow!(concat!("Failed to ", $name, ": {}"), e))
            .and_then(|status| {
                status.success().then_some(()).ok_or(anyhow!(
                    concat!("Failed to ", $name, ": program exited with code {}"),
                    status
                ))
            })?;
    };
}

fn main() -> anyhow::Result<()> {
    let Cli { uri } = Cli::parse();
    let parts = uri.into_parts();
    let path_and_query = parts.path_and_query.ok_or(anyhow!("No path specified"))?;

    let segs = path_and_query
        .path()
        .trim_matches('/')
        .split('/')
        .collect_vec();

    let (user, repo, _is_file, branch, path) = || -> Option<_> {
        let mut it = segs.iter().copied();
        let ret = (
            it.next()?,
            it.next()?,
            it.next()? == "blob",
            it.next()?,
            it.join("/"),
        );
        Some(ret)
    }()
    .ok_or(anyhow!("Invalid github url"))?;

    let repo_url = format!(
        "https://{}/{}/{}",
        parts.authority.unwrap().host(),
        user,
        repo,
    );

    let tmp = TempDir::new("get-git")?;
    let repo_path = tmp.path().join(repo);

    let pwd = env::current_dir()?;
    let target = pwd.join(path.rsplit('/').next().unwrap());

    if target.exists() {
        return Err(anyhow!("Target path not empty: {}", target.display()));
    }

    let ret = Command::new("git")
        .args(["clone", "-n", "--depth=1", "--filter=tree:0", &repo_url])
        .current_dir(tmp.path())
        .status();

    match ret {
        Err(e) => {
            if e.kind() == ErrorKind::NotFound {
                return Err(anyhow!("`git` not found in path, consider installing it?"));
            } else {
                return Err(anyhow!("Failed to clone: {}", e));
            }
        }
        _ => {}
    }

    exec!("set sparse checkout"; &repo_path, [
        "sparse-checkout",
        "set",
        "--sparse-index",
        "--no-cone",
        "--",
        &path,
    ]);

    exec!("checkout"; &repo_path, [
        "checkout", branch
    ]);

    fs::rename(repo_path.join(&path), target)?;

    tmp.close()?;

    Ok(())
}
