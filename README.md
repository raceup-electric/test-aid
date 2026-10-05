# taid

**taid** is a local terminal UI for browsing Git history, switching local
branches, and choosing which commits a repository records for its submodules.
It never fetches, pushes, creates commits, or rewrites history.

## Install

Install an existing local build without network access:

```sh
cargo build --locked
./install.sh --bin target/debug/taid
```

This installs to `~/.local/bin` and adds it to your shell's PATH if needed.
Set `TAID_INSTALL_DIR` to another absolute directory. To install a published
release, run `./install.sh` for the latest version or `./install.sh --tag
v0.1.0` for a specific version. Release downloads are checked against
`SHA256SUMS.txt`. Set `TAID_REPO=owner/name` to use a different release
repository. `--dry-run` prints the target and URLs without changing files.
The default release repository is `raceup-electric/test-aid`.
For a private release, set `GH_TOKEN` in your environment to a GitHub token
with repository Contents read access before running the installer. Private
downloads require `curl` and `jq`; the token is used only for the download and
is not saved by the installer. Public releases need no token.

Alternatively, install from source with Cargo:

```sh
cargo install --path . --locked
```

This places `taid` in Cargo's binary directory (normally `~/.cargo/bin`).
You need Rust 1.85 or newer and a C build toolchain. The build vendors libgit2;
it does not require a system libgit2 installation.

To build without installing:

```sh
cargo build --release --locked
./target/release/taid --version
```

## Usage

Run `taid` from a Git repository or any directory inside one:

```sh
cd path/to/repository
taid
```

The header shows the discovered repository root. With submodules, the left pane
lists the main repository first, followed by its submodules; the right pane
shows the selected repository's commits. Without submodules, the commit browser
uses the full width. Each commit shows a short hash, its timestamp in the
commit's own timezone, and its title.

Select a commit with `Enter` to mark it as a pending pick. Press `s` to review
the changes, then `Enter` or click **Apply** to save them. Closing the popup or
quitting without applying leaves the picks unapplied. Save handles each pick in
turn and reports successes and failures; failed picks remain available to retry.

- For a **submodule**, Save checks out the selected commit in its working
  directory with a detached HEAD and stages the new submodule pointer in the
  main repository. It does not commit that staged change.
- For the **main repository**, Save checks out the selected commit with a
  detached HEAD. Local branches and newer commits remain intact.
- Press `b` to switch the selected repository to a local branch. Switching a
  submodule's branch alone does not stage a new pointer in the main repository.

Checkout uses Git's safe mode: changes that would be overwritten cause an
error. Uninitialized submodules are listed but cannot be edited; initialize
one with `git submodule update --init <path>` outside taid.

## Keys and mouse

| Key | Action |
| --- | --- |
| `↑` / `↓` or `k` / `j` | Move in the focused list |
| `PgUp` / `PgDn`, `Home` / `End` | Move by page or to either end |
| `Tab` / `Shift+Tab` | Switch between repository and commit panes |
| `Enter` | Pick or unpick a commit; confirm a popup action |
| `b` | Open the local branch switcher |
| `/` | Filter commits by title or SHA (type to filter, `Enter` to keep, `Esc` to clear) |
| `s` | Review and apply pending picks |
| `r` | Refresh repository state |
| `?` | Open help |
| `q` / `Esc` | Close a popup or quit |
| `Ctrl+C` | Quit |

Click a repository or commit row to select and focus it. Click a branch row to
switch to that branch, or use the popup buttons to apply, cancel, or acknowledge
a result. The mouse wheel scrolls the focused list.

## Scope

Commit lists include history reachable from the current HEAD and local
branches. taid does not contact remotes, initialize submodules, or manage
nested submodules. See [PLAN.md](PLAN.md) for the development roadmap.

## License

MIT — see [LICENSE](LICENSE).
