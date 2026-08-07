use std::{path::PathBuf, str::FromStr};

use anyhow::{bail, Context, Result};
use http::Uri;
use percent_encoding::percent_decode_str;

use crate::{backend::Advertisement, TargetKind};

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ParsedSource {
    pub repository_url: String,
    pub repository: String,
    pub expected_kind: Option<TargetKind>,
    pub ref_and_path: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ResolvedRequest {
    pub repository_url: String,
    pub repository: String,
    pub reference: String,
    pub revision: String,
    pub path: String,
    pub expected_kind: Option<TargetKind>,
    pub output: PathBuf,
}

pub(crate) fn requires_advertisement(
    explicit_ref: Option<&str>,
    explicit_path: Option<&str>,
) -> bool {
    !matches!(
        (explicit_ref, explicit_path),
        (Some(reference), Some(_)) if is_direct_revision(reference)
    )
}

pub(crate) fn parse(source: &str) -> Result<ParsedSource> {
    let source_without_fragment = source.split_once('#').map_or(source, |(url, _)| url);
    let uri =
        Uri::from_str(source_without_fragment).with_context(|| format!("Invalid URL: {source}"))?;

    if uri.scheme_str() != Some("https") {
        bail!("Only HTTPS GitHub URLs are accepted");
    }

    let authority = uri.authority().context("The URL has no host")?;
    if authority.as_str().contains('@') {
        bail!("Credentials must not be embedded in the URL; use a Git configuration instead");
    }

    let host = authority.host().to_ascii_lowercase();
    let path = uri.path().trim_matches('/');
    let segments = if path.is_empty() {
        Vec::new()
    } else {
        path.split('/')
            .map(decode_url_segment)
            .collect::<Result<Vec<_>>>()?
    };
    if segments.len() < 2 {
        bail!("The URL must contain a repository owner and name");
    }

    let owner = validate_repository_component(&segments[0], "owner")?;
    let repo = validate_repository_component(segments[1].trim_end_matches(".git"), "repository")?;
    let (clone_authority, expected_kind, ref_and_path) = if host == "raw.githubusercontent.com" {
        if segments.len() < 4 {
            bail!("A raw.githubusercontent.com URL must identify a file");
        }
        (
            "github.com".to_owned(),
            Some(TargetKind::File),
            Some(segments[2..].join("/")),
        )
    } else {
        let clone_authority = if host == "www.github.com" {
            "github.com".to_owned()
        } else {
            authority.as_str().to_owned()
        };
        if segments.len() == 2 {
            (clone_authority, None, None)
        } else {
            let expected_kind = match segments[2].as_str() {
                "blob" | "raw" => TargetKind::File,
                "tree" => TargetKind::Directory,
                other => bail!(
                    "Unsupported GitHub URL form '/{other}/'; use a blob/tree URL or add --path"
                ),
            };
            if segments.len() < 5 {
                bail!("The GitHub URL does not contain both a reference and a repository path");
            }
            (
                clone_authority,
                Some(expected_kind),
                Some(segments[3..].join("/")),
            )
        }
    };

    Ok(ParsedSource {
        repository_url: format!("https://{clone_authority}/{owner}/{repo}.git"),
        repository: format!("{owner}/{repo}"),
        expected_kind,
        ref_and_path,
    })
}

pub(crate) fn resolve(
    parsed: ParsedSource,
    explicit_ref: Option<&str>,
    explicit_path: Option<&str>,
    output: Option<PathBuf>,
    advertisement: &Advertisement,
    current_dir: &std::path::Path,
) -> Result<ResolvedRequest> {
    let (reference, revision, path) = match explicit_path {
        Some(path) => {
            let reference = explicit_ref.unwrap_or("HEAD");
            (
                reference.to_owned(),
                resolve_revision(reference, advertisement)?,
                normalize_repo_path(path)?,
            )
        }
        None => {
            let ref_and_path = parsed.ref_and_path.as_deref().context(
                "The repository URL does not identify a file or directory; add --path and optionally --ref",
            )?;
            split_ref_and_path(ref_and_path, explicit_ref, advertisement)?
        }
    };
    let output = absolute_output_path(output, &path, current_dir)?;

    Ok(ResolvedRequest {
        repository_url: parsed.repository_url,
        repository: parsed.repository,
        reference,
        revision,
        path,
        expected_kind: parsed.expected_kind,
        output,
    })
}

fn split_ref_and_path(
    ref_and_path: &str,
    explicit_ref: Option<&str>,
    advertisement: &Advertisement,
) -> Result<(String, String, String)> {
    if let Some(reference) = explicit_ref {
        let path = strip_reference_prefix(ref_and_path, reference).with_context(|| {
            format!(
                "The URL path does not start with explicit reference '{reference}'; use a repository URL together with --ref and --path"
            )
        })?;
        return Ok((
            reference.to_owned(),
            resolve_revision(reference, advertisement)?,
            normalize_repo_path(path)?,
        ));
    }

    let mut matches = advertisement
        .refs
        .iter()
        .filter(|remote_ref| strip_reference_prefix(ref_and_path, &remote_ref.short_name).is_some())
        .collect::<Vec<_>>();
    matches.sort_by_key(|remote_ref| std::cmp::Reverse(remote_ref.short_name.len()));
    if let Some(longest) = matches.first() {
        let same_length = matches
            .iter()
            .take_while(|candidate| candidate.short_name.len() == longest.short_name.len())
            .copied()
            .collect::<Vec<_>>();
        let remote_ref = unique_remote_ref(&same_length, "the copied URL")?;
        let path = strip_reference_prefix(ref_and_path, &remote_ref.short_name)
            .expect("candidate was checked");
        return Ok((
            remote_ref.short_name.clone(),
            remote_ref.full_name.clone(),
            normalize_repo_path(path)?,
        ));
    }

    let (first, path) = ref_and_path
        .split_once('/')
        .context("The URL identifies a repository root, not a file or subdirectory")?;
    if is_full_object_name(first) {
        return Ok((
            first.to_owned(),
            first.to_owned(),
            normalize_repo_path(path)?,
        ));
    }

    bail!(
        "Could not distinguish the Git reference from the repository path in '{ref_and_path}'; use --ref and --path"
    )
}

fn resolve_revision(reference: &str, advertisement: &Advertisement) -> Result<String> {
    let reference = reference.trim();
    if reference.is_empty() {
        bail!("The Git reference cannot be empty");
    }
    if reference == "HEAD" {
        return advertisement
            .head_oid
            .clone()
            .context("The remote repository did not advertise a valid HEAD object ID");
    }
    if reference.starts_with("refs/") {
        return Ok(reference.to_owned());
    }

    let matches = advertisement
        .refs
        .iter()
        .filter(|remote_ref| remote_ref.short_name == reference)
        .collect::<Vec<_>>();
    if !matches.is_empty() {
        return Ok(unique_remote_ref(&matches, "the supplied reference")?
            .full_name
            .clone());
    }
    if is_full_object_name(reference) {
        return Ok(reference.to_owned());
    }

    bail!(
        "Reference '{reference}' is not an advertised branch or tag; use a full refs/... name or a full hexadecimal object ID"
    )
}

fn unique_remote_ref<'a>(
    matches: &[&'a crate::backend::RemoteRef],
    context: &str,
) -> Result<&'a crate::backend::RemoteRef> {
    match matches {
        [remote_ref] => Ok(*remote_ref),
        [] => bail!("No remote reference matched {context}"),
        _ => {
            let names = matches
                .iter()
                .map(|remote_ref| remote_ref.full_name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "The Git reference in {context} is ambiguous ({names}); use a repository URL with an explicit full --ref and --path"
            )
        }
    }
}

fn strip_reference_prefix<'a>(ref_and_path: &'a str, reference: &str) -> Option<&'a str> {
    ref_and_path.strip_prefix(reference)?.strip_prefix('/')
}

pub(crate) fn normalize_repo_path(path: &str) -> Result<String> {
    let path = path.trim_matches('/');
    if path.is_empty() {
        bail!("A repository-relative file or subdirectory path is required");
    }
    if path.contains('\\') || path.contains('\0') {
        bail!("Repository paths must use forward slashes and cannot contain NUL bytes");
    }
    if path
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        bail!("Repository paths cannot contain empty, '.' or '..' components");
    }
    Ok(path.to_owned())
}

fn absolute_output_path(
    output: Option<PathBuf>,
    repo_path: &str,
    current_dir: &std::path::Path,
) -> Result<PathBuf> {
    let output = output.unwrap_or_else(|| {
        PathBuf::from(
            repo_path
                .rsplit('/')
                .next()
                .expect("validated repository path"),
        )
    });
    let output = if output.is_absolute() {
        output
    } else {
        current_dir.join(output)
    };
    if output.file_name().is_none() {
        bail!("The output must name a file or directory, not a filesystem root");
    }
    Ok(output)
}

fn decode_url_segment(segment: &str) -> Result<String> {
    percent_decode_str(segment)
        .decode_utf8()
        .context("The URL contains a path segment that is not valid UTF-8")
        .map(|segment| segment.into_owned())
}

fn validate_repository_component<'a>(component: &'a str, label: &str) -> Result<&'a str> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains('/')
        || component.contains('\\')
    {
        bail!("Invalid repository {label}: '{component}'");
    }
    Ok(component)
}

fn is_full_object_name(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_direct_revision(reference: &str) -> bool {
    let reference = reference.trim();
    reference.starts_with("refs/") || is_full_object_name(reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::RemoteRef;

    fn advertisement() -> Advertisement {
        Advertisement {
            head_oid: Some("1111111111111111111111111111111111111111".to_owned()),
            refs: vec![
                RemoteRef {
                    full_name: "refs/heads/main".to_owned(),
                    short_name: "main".to_owned(),
                },
                RemoteRef {
                    full_name: "refs/heads/feature/slash".to_owned(),
                    short_name: "feature/slash".to_owned(),
                },
            ],
        }
    }

    #[test]
    fn parses_blob_tree_and_raw_urls() {
        let blob =
            parse("https://github.com/owner/repo/blob/main/docs/read%20me.md?plain=1#L10").unwrap();
        assert_eq!(blob.repository_url, "https://github.com/owner/repo.git");
        assert_eq!(blob.expected_kind, Some(TargetKind::File));
        assert_eq!(blob.ref_and_path.as_deref(), Some("main/docs/read me.md"));

        let tree = parse("https://github.com/owner/repo/tree/v1/docs").unwrap();
        assert_eq!(tree.expected_kind, Some(TargetKind::Directory));

        let raw = parse("https://raw.githubusercontent.com/owner/repo/v1/config.toml").unwrap();
        assert_eq!(raw.repository_url, "https://github.com/owner/repo.git");
        assert_eq!(raw.expected_kind, Some(TargetKind::File));
    }

    #[test]
    fn resolves_longest_slash_reference() {
        let result =
            split_ref_and_path("feature/slash/src/lib.rs", None, &advertisement()).unwrap();
        assert_eq!(result.0, "feature/slash");
        assert_eq!(result.1, "refs/heads/feature/slash");
        assert_eq!(result.2, "src/lib.rs");
    }

    #[test]
    fn resolves_head_and_rejects_abbreviated_object_ids() {
        assert_eq!(
            resolve_revision("HEAD", &advertisement()).unwrap(),
            "1111111111111111111111111111111111111111"
        );
        assert!(resolve_revision("0123456", &advertisement()).is_err());
    }

    #[test]
    fn skips_advertisement_only_for_an_explicit_direct_revision_and_path() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        assert!(!requires_advertisement(
            Some("refs/heads/main"),
            Some("src/lib.rs")
        ));
        assert!(!requires_advertisement(Some(oid), Some("src/lib.rs")));

        assert!(requires_advertisement(Some("main"), Some("src/lib.rs")));
        assert!(requires_advertisement(Some("HEAD"), Some("src/lib.rs")));
        assert!(requires_advertisement(None, Some("src/lib.rs")));
        assert!(requires_advertisement(Some("refs/heads/main"), None));
    }

    #[test]
    fn validates_repository_paths() {
        assert_eq!(normalize_repo_path("/src/lib.rs/").unwrap(), "src/lib.rs");
        assert!(normalize_repo_path("src/../secret").is_err());
        assert!(normalize_repo_path("src\\lib.rs").is_err());
        assert!(normalize_repo_path("").is_err());
    }
}
