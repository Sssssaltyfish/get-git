# get-git

Download one file or directory from a GitHub repository without checking out the rest of its working tree.

The default build contains two deliberately small backends. `auto` prefers Git 2.49 or newer because that path can combine an exact revision, shallow history, `blob:none` partial clone, no tag download, and checkout's bulk fetch of the selected content. If a suitable `git` executable is absent, it falls back to the embedded libgit2 backend. This default optimizes for the range of repositories that can be handled—including very large repositories such as Chromium or V8—rather than for the range of old host installations.

## Usage

Download a file from a copied GitHub URL:

```console
get-git https://github.com/rust-lang/rust/blob/master/README.md
```

Download a directory to an exact location:

```console
get-git https://github.com/owner/repo/tree/main/config --output ./vendor/config
```

Branch and tag names containing `/` are resolved against the remote advertisement. A repository URL can also be paired with an explicit reference and path:

```console
get-git https://github.com/owner/repo \
  --ref feature/linux \
  --path src/config \
  --output ./config
```

Use a fully qualified ref (or a full object ID) with `--path` when startup latency matters. In that unambiguous form, get-git goes directly to the clone and skips the separate remote-reference advertisement:

```console
get-git https://github.com/owner/repo \
  --ref refs/heads/main \
  --path src/config \
  --output ./config
```

PowerShell uses a backtick instead of `\` for line continuation. Raw GitHub file URLs are accepted too:

```console
get-git https://raw.githubusercontent.com/owner/repo/main/config/default.toml
```

Run `get-git --help` for the complete option list.

## Backends and large repositories

`--backend auto` is the default:

- A system Git 2.49+ is preferred. It uses `clone --revision`, `--depth=1`, `--no-tags`, `--filter=blob:none`, and `--no-checkout`; only the selected literal path is checked out. Directory blobs are requested in one `fetch --stdin` batch before checkout.
- If system Git is missing or older, the embedded `git2` backend is used. It needs no `git` executable and performs a depth-one fetch. If a transport explicitly rejects shallow fetches, it alone falls back to a full fetch.

The embedded backend cannot currently request a partial-clone object filter through git2's high-level fetch API. Consequently, `auto` intentionally chooses modern system Git for giant repositories. Use `--backend git2` to require a no-executable run, or `--backend system` to require the optimized external path and its version check.

The system Git version is checked once while selecting the backend. Short branch or tag names and copied GitHub URLs still need a remote advertisement for correct slash-containing-ref disambiguation; an explicit `refs/...` name or full object ID together with `--path` does not.

Git documents partial clone as an optimization for extremely large repositories, and documents checkout as bulk-prefetching required missing blobs. `blob:none` keeps tree metadata available for reliable path traversal while omitting every file body until needed. See the official [`git clone` options](https://git-scm.com/docs/git-clone), [partial-clone design](https://git-scm.com/docs/partial-clone), and [filter definitions](https://git-scm.com/docs/git-rev-list#Documentation/git-rev-list.txt---filterltfilter-specgt).

get-git deliberately does not query a hosting API to guess a different filter from repository size. Git's [protocol-v2 reference advertisement](https://git-scm.com/docs/gitprotocol-v2#_ls_refs) has no recursive tree-size field; its optional `object-info` command reports only the size of object IDs the client already knows. GitHub's [recursive Trees API](https://docs.github.com/en/rest/git/trees#get-a-tree) sends the tree listing itself and can truncate it at 100,000 entries or 7 MB. By the time a `blob:none` clone can count the selected tree locally, choosing the initial tree filter is already complete, and checkout already bulk-fetches missing blobs. A size-only threshold would therefore add a request without reliably accounting for target depth, network latency, or server behavior.

The 2.49 floor is intentional: [`clone --revision`](https://git-scm.com/docs/git-clone/2.49.0#Documentation/git-clone.txt---revisionltrevgt) first appears in the 2.49 manual and directly expresses “fetch this revision and nothing else.” Git 2.48's [`git clone` manual](https://git-scm.com/docs/git-clone/2.48.0) does not provide that option. Reimplementing it as an older `init`/`fetch` sequence would expand the host-version range while weakening the exact, tested large-repository path.

## Isolated Git configuration

If no configuration option is supplied, get-git uses this bundled configuration:

```gitconfig
[http]
    sslVerify = true
```

It deliberately contains no machine paths, proxy, credential helper, identity, URL rewrite, or performance override. That makes default behavior reproducible across machines, although private repositories and managed networks may require an explicit configuration source.

The following options are mutually exclusive:

- `--git-config PATH` uses exactly the named file.
- `--project-config` uses the nearest repository's `.git/config` and, when present, `config.worktree`.
- `--user-config` uses XDG and home-directory Git configuration files in normal precedence order.
- `--system-config` uses a discovered platform system configuration file.

get-git writes a temporary wrapper that includes only the selected source files. The system backend sets `GIT_CONFIG_GLOBAL` to that wrapper and disables normal system configuration; the embedded backend attaches the same wrapper to its temporary repository. Git's documented configuration locations and precedence are described in [`git-config`](https://git-scm.com/docs/git-config#FILES).

Use `--user-config` when you want an existing credential helper, proxy, Git LFS filter, or `url.*.insteadOf` rule. Use `--git-config` when reproducibility requires a purpose-built file. Conditional includes still evaluate in get-git's temporary repository context, so a condition tied to the original repository path may not match.

## Work and output directories

`--work-dir DIR` (also accepted as `--temp-dir`) chooses the base directory for an automatically removed, uniquely named work directory. The base itself is retained. Without it, the platform temporary directory is used.

Output is staged beside the destination so the final move stays on one filesystem. Existing output is rejected unless `--force` is present. With `--force`, the old path is moved aside only after the replacement has been fully downloaded; it is restored if the final move fails.

Repository paths are always literal. Empty components, `.`, `..`, backslashes, NUL bytes, embedded URL credentials, and non-HTTPS source URLs are rejected. A copied `blob` URL must resolve to a file and a copied `tree` URL must resolve to a directory.

On Unix, executable bits and symbolic links are preserved. The embedded backend represents a symbolic link as a regular file containing its target on Windows, matching the non-privileged Git for Windows convention. A selected submodule is rejected; nested gitlinks are omitted because their content belongs to another repository. Git does not track empty directories.

The embedded backend writes Git blob contents directly. The system backend uses checkout, so filters configured by the selected config source can transform output; for example, selecting user configuration may activate Git LFS.

## Building and feature gates

The default release build enables both backends and vendors OpenSSL for the embedded HTTPS implementation:

```console
cargo build --release
```

Build a self-contained backend that never invokes environment Git:

```console
cargo build --release --no-default-features \
  --features backend-git2,vendored-openssl
```

Build the smallest variant and require Git 2.49+ at runtime:

```console
cargo build --release --no-default-features --features backend-system
```

Omit `vendored-openssl` from an embedded build to link against a usable platform OpenSSL instead. `backend-system` and `backend-git2` are compile-time capability gates; the large-repository optimization remains enabled whenever the system backend is selected. At least one backend must be enabled.

The source is portable across Windows and Linux. CI runs the unit and command-level end-to-end suites on both systems, then compiles and lints the default, embedded-only, and system-only feature combinations.

## Tests

```console
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
```

Command-level tests live under `tests/`. They create a real Git object database, exercise both backends, clear `PATH` for the embedded run, select explicit and project configuration, choose a work-directory base, and verify transactional replacement. They do not depend on GitHub or mutate the source tree.

## License

MIT
