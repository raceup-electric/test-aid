# taid

**taid** is a terminal UI for the driverless team's Git repositories and module
configurations. Browse history, switch branches, choose submodule commits, edit
YAML parameters through typed forms, and pull repository updates. It never pushes, creates
commits, or rewrites history.

## Install

From a checkout of this repository, install the latest release for your machine:

```sh
./install.sh
```

The installer detects the OS and CPU architecture, selects the matching binary,
checks it against `SHA256SUMS.txt`, and installs it to `~/.local/bin`. It adds
that directory to your shell's PATH if needed; open a new shell afterwards.
Run `./install.sh --dry-run` to see the selected target without installing.
Running the installer again replaces the existing binary with the selected
version; `./install.sh` updates it to the latest **published release**. It does
not build the current checkout automatically.

This repository is currently **private**, so first authenticate with GitHub CLI
(`gh auth login`) or supply a token with repository **Contents: read** access:

```sh
GH_TOKEN=your_token ./install.sh
```

With an authenticated `gh` CLI, the first `./install.sh` command works as-is;
the installer uses its token automatically. Private downloads require `curl`
and `jq`. SSH access to the repository alone does not authenticate release
downloads. The token is used only for the download and is not saved by the
installer. Public releases need no token.

Use `./install.sh --tag v0.1.0` to pin a version,
`TAID_INSTALL_DIR=/absolute/path ./install.sh` to choose the destination, or
`TAID_REPO=owner/name ./install.sh` to use a different release repository.
To build and install the current checkout, including changes that have not
been released yet:

```sh
cargo build --release --locked
./install.sh --bin target/release/taid
```

### Release builds

The [latest release](https://github.com/raceup-electric/test-aid/releases/latest)
contains one archive for each supported OS and architecture. These badges
track the **individual target jobs** in the latest tagged release build, not
just the overall release workflow. They update after each release build;
[![CI](https://github.com/raceup-electric/test-aid/actions/workflows/ci.yml/badge.svg)](https://github.com/raceup-electric/test-aid/actions/workflows/ci.yml)
tracks format, lint, and tests on `main` separately.

| OS | Architecture | Release target | Build |
| --- | --- | --- | --- |
| Linux | x86-64 | `x86_64-unknown-linux-musl` | [![Linux x86-64 build](https://github.com/raceup-electric/test-aid/actions/workflows/release-linux-x86_64.yml/badge.svg)](https://github.com/raceup-electric/test-aid/actions/workflows/release-linux-x86_64.yml) |
| Linux | ARM64 | `aarch64-unknown-linux-musl` | [![Linux ARM64 build](https://github.com/raceup-electric/test-aid/actions/workflows/release-linux-aarch64.yml/badge.svg)](https://github.com/raceup-electric/test-aid/actions/workflows/release-linux-aarch64.yml) |
| Linux | ARMv7 | `armv7-unknown-linux-musleabihf` | [![Linux ARMv7 build](https://github.com/raceup-electric/test-aid/actions/workflows/release-linux-armv7.yml/badge.svg)](https://github.com/raceup-electric/test-aid/actions/workflows/release-linux-armv7.yml) |
| macOS | Intel | `x86_64-apple-darwin` | [![macOS Intel build](https://github.com/raceup-electric/test-aid/actions/workflows/release-macos-x86_64.yml/badge.svg)](https://github.com/raceup-electric/test-aid/actions/workflows/release-macos-x86_64.yml) |
| macOS | Apple silicon | `aarch64-apple-darwin` | [![macOS Apple silicon build](https://github.com/raceup-electric/test-aid/actions/workflows/release-macos-aarch64.yml/badge.svg)](https://github.com/raceup-electric/test-aid/actions/workflows/release-macos-aarch64.yml) |
| Windows | x86-64 | `x86_64-pc-windows-msvc` | [![Windows x86-64 build](https://github.com/raceup-electric/test-aid/actions/workflows/release-windows-x86_64.yml/badge.svg)](https://github.com/raceup-electric/test-aid/actions/workflows/release-windows-x86_64.yml) |
| Windows | ARM64 | `aarch64-pc-windows-msvc` | [![Windows ARM64 build](https://github.com/raceup-electric/test-aid/actions/workflows/release-windows-aarch64.yml/badge.svg)](https://github.com/raceup-electric/test-aid/actions/workflows/release-windows-aarch64.yml) |

Because the repository is private, GitHub's workflow badges are visible to
people with access to it, but cannot be embedded on external sites.

Alternatively, install from source with Cargo:

```sh
cargo install --path . --locked
```

This places `taid` in Cargo's binary directory (normally `~/.cargo/bin`).
You need Rust 1.85 or newer and a C build toolchain. The build vendors libgit2;
it does not require a system libgit2 installation. Remote checks and pulls
require the `git` executable and use your existing SSH configuration and
credential helpers. Authenticate outside taid first; it does not show credential
prompts inside the terminal UI.

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

The default view is **Repositories**. Press `Ctrl+R`, then `R` to return to it;
press `Ctrl+C`, then `C` to open **Configurations**. Both shortcuts also work
with Ctrl held for the second letter; press the second key within two seconds.
The header identifies the active view, and the footer shows its shortcuts.

The red `● dirty` marker means the worktree or staging area differs from HEAD:
edited files, untracked files, staged changes, or changed submodule checkouts.
Saving config parameters makes that module dirty until the changes are committed
or reverted. The marker does not mean that new remote commits are available;
those use the yellow `[new]` badge.

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

### Module configurations

The Configurations view lists only initialized submodules with `.yaml`/`.yml` files inside a
`config` directory. Discovery includes nested ROS packages such as
`lateral_controller/pure_pursuit_controller/config` and
`coupled_controls/nmpc_spatial/config`. Git metadata, build/install/log folders,
vendor/thirdparty trees, nested repositories, and symlinks are skipped.

Choose a module, use `Tab` to focus its files, and press `Enter` (or click a file)
to open its parameter form. There is no free-text YAML editor. Existing YAML
structure and scalar types define the form's schema: parameter names, nesting,
types, and array lengths are fixed. Standalone validation schemas are not present
in this stack, so domain constraints such as units, ranges, or enumerations are
not inferred from prose comments.

The form shows parameter names, types, current values, and comments as help.
Use `/` to search names or descriptions. `Enter`/`Space` toggles booleans or opens
a value input for numbers/text; typing replaces the selected current value.
`Enter` accepts a value, `Esc` cancels that value input, and `r` resets the selected
parameter to its last saved value. Array elements are edited individually.
Whole-number fields require integers; numeric fields require finite numbers;
legacy `Yes`/`No` and `On`/`Off` boolean values used by ROS/PyYAML are supported
(an explicit `%YAML 1.2` directive uses core scalar types instead);
text fields are quoted/escaped automatically. Pasted content must be a single
parameter value. Nulls, aliases, tagged/anchored and multiline values are
read-only; merge keys and complex/duplicate mapping keys are refused.

`Ctrl+S` validates pending input and the resulting YAML schema, then writes values
directly to the file. You can also click **Save** or **Back** in the form.
Saving does not stage or commit changes. Saves preserve comments,
line endings, file permissions and all untouched formatting. UTF-8 files up to
2 MiB are supported. Saving refuses to overwrite a file changed by another process.

`Esc` closes a saved form. Unsaved changes block closing, quitting and view
switching; save with `Ctrl+S` or use `Ctrl+D` to discard and close.

### Remote updates and pulling

taid does not fetch or pull at startup. A yellow `[new]`
badge indicates commits on the selected local branch or its fetched upstream
that are missing from the current checkout. Badges reflect the latest successful
fetch, including local branch commits when browsing an older detached commit.
They use locally stored references and cannot detect new remote commits until
a confirmed pull (or a fetch performed outside taid) refreshes those references.
When reopening a detached checkout, the branch is inferred only if exactly one
local branch contains that commit. Otherwise select a branch with `b`.

Press `Ctrl+P`, then `P` to open a confirmation popup. Press `Enter`/`y` or click
**Pull** to fetch and fast-forward the main repository and every initialized
submodule; `Esc`/`q`/`n` or **Cancel** closes it without any remote operation.
Each target reports its own result, and failures do not
stop the rest. Repositories without remotes are reported as skipped. Detached
HEADs, missing upstreams, local file changes, and diverged branches are reported
without overwriting files or creating merges. Apply or clear pending commit
picks and close the config form before pulling. Wait for remote operations
to finish before quitting. Pulls temporarily block changes to commits/config
files.

Pulling a submodule updates its working checkout; it does not stage its pointer
in the main repository. Submodules are handled individually with recursive
checkout disabled. Newly added/uninitialized submodules must be initialized
outside taid. Press `n` to reopen the remote results and `↑`/`↓` to scroll them.

Normal browsing works offline; remote operations run only after you confirm
the pull shortcut.

## Keys and mouse

| Key | Action |
| --- | --- |
| `Ctrl+R`, `R` | Repository management view (default) |
| `Ctrl+C`, `C` | Module configurations view |
| `Ctrl+P`, `P` | Confirm fetching and fast-forwarding all repositories |
| `↑` / `↓` or `k` / `j` | Move in the focused list |
| `PgUp` / `PgDn`, `Home` / `End` | Move by page or to either end |
| `Tab` / `Shift+Tab` / `←` / `→` | Switch panes |
| `Enter` | Pick/unpick a commit, open a YAML form, edit/toggle a parameter, or confirm a popup |
| `b` | Open the local branch switcher |
| `/` | Filter commits by title or SHA (type to filter, `Enter` to keep, `Esc` to clear) |
| `s` | Review and apply pending picks |
| `r` | Refresh repository state |
| `n` | Show remote operation results |
| `?` | Open help |
| `q` / `Esc` | Close a popup or quit |
| `Ctrl+Q` | Quit (protecting unsaved config changes) |
| `/` (form) | Find parameters by name or comment |
| `r` (form) | Reset the selected parameter to its last saved value |
| `Ctrl+S` (form) | Validate and save parameter values |
| `Esc` (form) | Cancel a value input/search or close a saved form |
| `Ctrl+D` (form) | Discard unsaved parameter values and close |

Inside a value input, `←`/`→`, `Home`/`End` move the cursor,
`Backspace`/`Delete` remove characters, and `Ctrl+U` clears it. Typing outside a
value input cannot edit YAML. `Ctrl+C` starts the config shortcut instead of quitting.

Click a repository or commit row to select and focus it. Click a branch row to
switch to that branch, or use the popup buttons to apply, cancel, or acknowledge
a result. The mouse wheel scrolls the focused list.

## Scope

Commit lists include history reachable from the current HEAD and local
branches. Remote operations fetch refs and only fast-forward the current branch.
taid does not initialize submodules or manage nested submodules. See
[PLAN.md](PLAN.md) for the original development roadmap.

## License

MIT — see [LICENSE](LICENSE).
