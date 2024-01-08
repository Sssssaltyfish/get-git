use std::{io::ErrorKind, process::Command};

use anyhow::anyhow;
use clap::Parser;
use http::{uri::PathAndQuery, Uri};
use itertools::Itertools;

#[derive(Parser)]
struct Cli {
    pub uri: Uri,
}

fn main() -> anyhow::Result<()> {
    let Cli { uri } = Cli::parse();
    let mut parts = uri.into_parts();
    let path_and_query = parts.path_and_query.ok_or(anyhow!("No path specified"))?;

    let mut segs = path_and_query
        .path()
        .trim_end_matches('/')
        .split('/')
        .collect_vec();

    let mut is_dir = true;
    let idx = segs
        .iter()
        .rposition(|&s| match s {
            "tree" => true,
            "blob" => {
                is_dir = false;
                true
            }
            _ => false,
        })
        .ok_or(anyhow!("Not a github repo"))?;

    let branch = segs[idx + 1];
    match branch {
        "master" | "main" => {
            segs[idx] = "trunk";
            segs.remove(idx + 1);
        }
        _ => {
            segs[idx] = "branches";
        }
    }

    let final_path = segs.iter().join("/") + path_and_query.query().unwrap_or_default();
    let new_path_and_query = PathAndQuery::from_maybe_shared(final_path)?;
    parts.path_and_query = Some(new_path_and_query);

    let svn_uri = Uri::from_parts(parts)?;

    let res = Command::new("svn")
        .args([
            if is_dir { "checkout" } else { "export" },
            &svn_uri.to_string(),
        ])
        .status();

    match res {
        Ok(stat) => {
            if !stat.success() {
                std::process::exit(stat.code().unwrap_or(-1));
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            Err(anyhow!("`svn` not found in PATH, consider installing it?"))?;
        }
        e @ Err(_) => {
            e?;
        }
    }

    Ok(())
}
