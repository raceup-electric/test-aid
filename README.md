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
those use the yellow `[new: N]` badge.

Select a commit with `Enter` to mark it as a pending pick. Press `s` to review
the changes, write the required note, then `Enter` to finish it and `Enter` again
to apply. Closing the popup or
quitting without applying leaves the picks unapplied. Save handles each pick in
turn and reports successes and failures; failed picks remain available to retry.

- For a **submodule**, Save checks out the selected commit in its working
  directory with a detached HEAD and stages the new submodule pointer in the
  main repository. It does not commit that staged change.
- For the **main repository**, Save checks out the selected commit with a
  detached HEAD. Local branches and newer commits remain intact.
- Press `b` to switch the selected repository to a local or fetched remote branch.
  Press `f` in the picker to fetch remote branches for all repositories without
  changing any checkout. Remote rows have a `[remote]` label. Selecting one
  creates and checks out a local branch with upstream tracking; existing local
  branches are never overwritten (select the local row instead). Switching a
  submodule's branch alone does not stage a new pointer in the main repository.

Note input opens automatically when saving commit picks, confirming a pull, or
selecting a branch with `Enter`. Write 1–500 characters, including at least one
non-whitespace character. `Enter` finishes typing; a second `Enter` confirms the
action. `Esc` cancels and discards the draft. No separate `n` key is needed;
`n` can still reopen a finished note for revision before confirming.
One batch confirmation displays one note grouping all affected repositories,
including their successful/failed/partial results. Separate confirmations remain
separate notes, even with identical wording. Config-save and restore notes are
collected in the same session.

Checkout uses Git's safe mode: changes that would be overwritten cause an
error. Uninitialized submodules are listed but cannot be edited; initialize
one with `git submodule update --init <path>` outside taid.

### Module configurations

The Configurations view lists only initialized submodules with `.yaml`/`.yml` files inside a
`config` directory. Discovery includes nested ROS packages such as
`lateral_controller/pure_pursuit_controller/config` and
`coupled_controls/nmpc_spatial/config`. Git metadata, build/install/log folders,
vendor/thirdparty trees, nested repositories, and symlinks are skipped.

Choose a module, use `Tab` to focus its files, and press `Enter`
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

`Ctrl+S` validates pending input and opens a review popup listing each changed
parameter's last-saved and proposed values, with the required note input already
active (1–500 characters, including at least one non-whitespace character).
`Enter` finishes the note; `Enter` again confirms saving. `Esc` returns to the
form without losing edits. After finishing the note, use `↑`/`↓`, `PgUp`/`PgDn`,
`Home`/`End` to scroll the preview; `n` reopens the note for revision.
Saving does not stage or commit changes. Saves preserve comments,
line endings, file permissions and all untouched formatting. UTF-8 files up to
2 MiB are supported. Saving refuses to overwrite a file changed by another process.

`Esc` closes a saved form. Unsaved changes block closing, quitting and view
switching; save with `Ctrl+S` or use `Ctrl+D` to discard and close.

### Global configuration history and recovery

Every changed config save records full before/after file snapshots, parameter
changes, UTC timestamp, module and stack-relative file path, branch/checkout commit, required note, and
the source of a restore. New saves also carry a unique **taid session ID**:
one invocation groups every confirmed config save and repository action across
modules and files. Branch switches, applied commit picks and confirmed pulls
have durable before/after checkout records, including failed/partial outcomes.
Individual saves remain durable immediately, not buffered until quitting.
The first save therefore records the original baseline.
No-op saves do not create entries. Records are retained without automatic pruning.
Edits made outside taid are not independently tracked; the next taid save records
the on-disk version it started from.

Press `h` from Repositories, Configurations, or a clean parameter form to browse
saved configuration and repository changes in the opened checkout, including
its submodules. The top-level list shows sessions, their time spans, affected
module/file/save counts, description and **active**, **closed**, or **interrupted**
state. Separate concurrent taid instances have separate session IDs; an OS-held
lock distinguishes a live instance from an interrupted one. A normal exit marks
the session closed. Sessions without saves or repository actions do not appear.
Branch-only and pull-only sessions appear too, including no-op/failed pulls.
Older entries stay under **Legacy saves**, since their session boundaries cannot
be reconstructed (a checkout containing only legacy entries opens that list).

Session → repository action / file → individual save navigation keeps intermediate versions available:

- `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End` select entries.
- `Enter` expands a session into repository actions and files, then a file into individual saves.
  `Backspace` returns to the parent level.
- `n` appends an overall session note (up to 500 characters); `Enter` saves it,
  `Esc` cancels. Existing notes are retained.
- The selected session has a **Granular changes** pane on the left and a
  **Session notes** pane on the right. All config, branch, commit, pull, restore,
  and overall notes appear newest first with their action/file context, including
  when inspecting a file or action within that session. Notes wrap over multiple
  lines and scroll independently. On narrow terminals the panes stack vertically.
- `/` searches module/file paths, session descriptions, save notes, branches and commit IDs; `Enter` keeps the
  search, `Esc` clears it, and `Ctrl+U` clears the search input.
- `f` toggles between the selected file's history and all files.
- `R` reloads the catalogue; `w` shows migration/recovery warnings.
- The default comparison shows **first before → last after** per file for a
  session/file row. For `gain: 1 → 2 → 3`, the session diff is `1 → 3` while both
  saves remain accessible. Individual saves show **previous → saved**.
  Repository diffs show branch/checkout commit and staged submodule gitlinks
  separately, with seven-character commit hashes and titles. Unavailable commit
  objects show the short hash and a title-unavailable label; audit and restore
  records retain full hashes. Local branch tips and fetched remote refs are audit context only;
  restoring never rewinds them. Action rows retain requests and results, even
  if the checkout did not change.
- `c` switches to **current → selected**, previewing what a restore would change.
- `b` chooses the selected entry's **before** or **after** snapshot as the restore
  target. The first entry's before snapshot is the baseline; the latest entry's
  before snapshot undoes the latest save.
- `Tab`/`Shift+Tab` cycles between entries, changes, and notes.
  The navigation keys scroll the focused pane.
- `r` on a session/action/file row opens a review covering **all recorded targets in that row**;
  Required note input opens immediately: `Enter` finishes typing, then `Enter`
  confirms the displayed writes. `Esc` cancels without writing. One grouped note
  documents every target in a batch.
  Search/filtering hides rows but does not secretly
  narrow a session's restore scope; expand and select a file for a smaller restore.
- `r` on an individual save loads pending form values; `Ctrl+S` reviews and
  confirms normally. Every changed restored file adds an individual save under
  the current taid session, so later versions remain available.
- `u` toggles unfinished multi-file restores. `r` reviews resuming their **after**
  targets; `b` selects their **before** targets to review rollback. `w` shows
  observed per-file outcomes. Active/corrupt plans cannot be restored. Successfully
  confirmed recovery marks the old plan resolved without deleting its snapshots.
- `h`/`Esc`/`q` closes History.

Restoring changes only existing compatible parameter values, preserving the
current file's comments, formatting and permissions. Missing/renamed keys, type
changes, changed array lengths and differing read-only values block restoration;
the UI names incompatible fields and lets you compare before deciding. Pending
edits prevent a blind restore. Later/external parameter edits are explicitly
flagged in the whole-session review, with their actual current values shown.
Changes occurring after the preview invalidate confirmation; reopen and review.

Repository restoration uses safe local checkouts and restores explicitly reviewed
staged gitlinks. It does not pull, force checkout, create/delete branches, or rewind
branch/remote refs. The recorded branch is reattached only if it still points at
the recorded commit; otherwise the preview warns and restores a detached HEAD.
Dirty/staged tracked files block restoring a different commit; a branch change
at the same commit preserves them. Active Git operations, missing local commits,
untracked collisions, unsafe paths and preview drift also block restoration. Save or
restore config rows separately first, or commit/stash outside taid. If a checkout
would replace a selected config's revision/schema, the whole-session restore is
blocked: restore the repository and config rows separately with fresh previews.
No temporary discard/stash is performed. Historical commits are not pinned by
private Git refs and may become unavailable after Git garbage collection.
Operations performed outside taid and repository actions from older taid versions
cannot be reconstructed retroactively.

New history resides only under the **opened checkout's**
`<actual Git directory>/taid/config-history` (usually
`driverless-stack/.git/taid/config-history`). Submodule saves go into that same
central store, not their own Git directories. Launching taid inside a folder of
the checkout uses its discovered repository root; opening a submodule on its own
instead gives it an independent history scope. Git-file/worktree checkouts use
their actual administrative directory.
Session envelopes/descriptions reside alongside it in `taid/config-sessions`,
repository intents/outcomes in `taid/repository-history`, and multi-target restore
plans/progress in `taid/config-restores`, all under the
same opened checkout's Git administrative directory and never pushable.

Existing per-submodule journals are imported automatically into the central
catalogue when history is opened or the relevant config is opened. Originals are
retained, verified snapshots and IDs are preserved, and repeated imports do not
duplicate entries. Only initialized submodules can be scanned; `R` reloads after
initialization. Failed imports are reported under `w`, without deleting records.

History is not placed in the worktree or in Git objects, refs, commits,
notes or the index. **Git push/clone does not transfer it.** This is global within
the opened checkout, not across unrelated stacks or machines. History files/directories use owner-only
permissions on Unix; Windows uses the checkout's existing filesystem ACLs.
Moving/copying the whole Git metadata directory can of course copy the history;
losing that directory loses the local history too.

Saving first locks the per-file journal, writes and syncs the complete recovery
record, then atomically replaces the YAML and finalizes the journal. History
storage/locking failures prevent starting a save. File contents are synced on all
platforms; directory entries are additionally synced on Unix. Durability still
depends on the filesystem/hardware honoring those operations.

On reopening the form or global history, interrupted saves are checked against the actual file:
matching the after snapshot finalizes the history; matching before or neither
retains an interrupted entry with an explicit warning. Recovery **never replaces
the YAML automatically**. Incomplete/corrupt records remain visible, with corrupt
restores blocked by integrity checks. A corrupt pending recovery record blocks
new saves until its journal issue is resolved; healthy entries remain browsable.
Two taid instances cannot write the same config concurrently through taid's
journal. Non-cooperating external editors are checked before replacement, but
cannot participate in that lock; no application can guarantee exclusion against
an arbitrary external write racing the final filesystem operation.

A session/action/file restore validates repository states and files before any write,
takes a shared checkout lock as well as all affected
per-file locks in deterministic order (including unchanged targets), rechecks
the preview baselines, and syncs a complete multi-file before/after recovery
plan before changing a checkout or replacing any YAML. Each repository action
and changed file also uses its normal journal. Repository intents are synced
before Git runs; an interrupted action remains visible with an unknown outcome,
and its before snapshot can be reviewed without guessing an after state.
Multi-target writes are **not one atomic filesystem operation**: a failure stops
remaining writes and retains both the full plan and per-file completion markers.
Opening history only reports observed disk states; it never automatically
resumes or rolls back. Use `h` → `u` to review recovery, including files the
interrupted write loop never reached. Plans are integrity-checked and retained
after completion/resolution; no automatic pruning occurs. A recovery plan is
limited to 32 MiB serialized (individual snapshots remain limited to 2 MiB);
an oversized plan refuses to start writing.

### Remote updates and pulling

taid does not fetch or pull at startup. A yellow `[new: N]`
badge indicates commits on the selected local branch or its fetched upstream
that are missing from the current checkout. Badges reflect the latest successful
fetch, including local branch commits when browsing an older detached commit.
They use locally stored references and cannot detect new remote commits until
a confirmed pull (or a fetch performed outside taid) refreshes those references.
When reopening a detached checkout, the branch is inferred only if exactly one
local branch contains that commit. Otherwise select a branch with `b`.
`[ahead: N]` means the local branch has commits absent from its stored upstream.
`[diverged]` means both branches have unique commits, so a fast-forward is
impossible. `[detached]`, `[branch ?]`, `[no upstream]` and `[unborn]`
distinguish checkouts that cannot be pulled normally. The commit-pane summary
shows exact ahead/behind counts and checkout-missing commits. Ahead/behind
compare the selected local branch tip to its upstream; `[new: N]` compares the
checkout to the union of that branch's and upstream's history. Divergence takes
precedence over the new/ahead badge.
Narrow list rows show the highest-priority state so module names remain readable;
the summary explains additional states such as a detached checkout behind a branch.

Press `Ctrl+P`, then `P` to open a confirmation popup with note input active.
Write a required note and press `Enter` to finish it, then `Enter`/`y` to fetch
and fast-forward the main repository and every initialized submodule's current
branch. `Esc` cancels without any remote operation. To discover branches without
pulling the current checkout, use `b`, then `f`.
To pull only one repository, highlight it and press `p` (in Configurations, this
uses the highlighted module). The confirmation names that repository, and note
input opens immediately. Only that repository is fetched and its current branch
fast-forwarded; other repositories are untouched. `Ctrl+P`, `P` still
pulls the whole stack. The same `p` action is available in the branch picker.
Each target reports its own result, and failures do not
stop the rest. Repositories without remotes are reported as skipped. Detached
HEADs, missing upstreams, local file changes, and diverged branches are reported
without overwriting files or creating merges. Apply or clear pending commit
picks for the repositories being pulled and close the config form before pulling.
Pending picks in other repositories remain available after a single-repository
pull. Wait for remote operations
to finish before quitting. Pulls temporarily block changes to commits/config
files.
Press `Ctrl+X` during a remote operation to cancel an active fetch and skip
remaining repositories. An already-started checkout is allowed to finish;
successful earlier pulls are not rolled back. Commands have a 60-second deadline
including output-pipe closure, bounded diagnostic capture and helper cleanup.
Timeouts during checkout are reported as potentially partial: inspect `git status`
before retrying; taid does not automatically reset the repository. Existing
merge/rebase/cherry-pick operations are refused. Diagnostics redact URL credentials
and remove terminal control characters.

Pulling a submodule updates its working checkout; it does not stage its pointer
in the main repository. Submodules are handled individually with recursive
checkout disabled. Newly added/uninitialized submodules must be initialized
outside taid. Press `n` to reopen the remote results and `↑`/`↓` to scroll them.

Normal browsing works offline; remote operations run only after you confirm
the pull shortcut.

## Keys

| Key | Action |
| --- | --- |
| `Ctrl+R`, `R` | Repository management view (default) |
| `Ctrl+C`, `C` | Module configurations view |
| `Ctrl+P`, `P` | Confirm fetching and fast-forwarding all repositories |
| `p` (repositories/configurations/branch picker) | Confirm pulling only the selected repository/module |
| `↑` / `↓` or `k` / `j` | Move in the focused list |
| `PgUp` / `PgDn`, `Home` / `End` | Move by page or to either end |
| `Tab` / `Shift+Tab` / `←` / `→` | Switch panes |
| `Enter` | Pick/unpick a commit, open a YAML form, edit/toggle a parameter, or confirm a popup |
| `b` | Open the local/remote branch switcher |
| `f` (branch picker) | Fetch remote branches for all repositories without switching checkouts |
| `/` | Filter commits by title or SHA (type to filter, `Enter` to keep, `Esc` to clear) |
| `s` | Review and apply pending picks |
| `r` | Refresh repository state |
| `n` | Show remote operation results |
| `?` | Open help |
| `q` / `Esc` | Close a popup or quit |
| `Ctrl+Q` | Quit (protecting unsaved config changes) |
| `/` (form) | Find parameters by name or comment |
| `r` (form) | Reset the selected parameter to its last saved value |
| `Ctrl+S` (form) | Validate and review parameter changes before saving |
| `h` (repositories/configs/clean form) | Global sessions (repository actions + config saves); `Enter` expand, `Backspace` up, `/` search, `f` file/all, `n` append note, `Tab` entries/changes/notes, `c` compare, `b` target, `r` review restore, `R` reload, `w` warnings, `u` recovery |
| `Ctrl+X` | Cancel a fetch/skip remaining repositories; finish an active checkout |
| `Esc` (form) | Cancel a value input/search or close a saved form |
| `Ctrl+D` (form) | Discard unsaved parameter values and close |

Inside a value input, `←`/`→`, `Home`/`End` move the cursor,
`Backspace`/`Delete` remove characters, and `Ctrl+U` clears it. Typing outside a
value input cannot edit YAML. `Ctrl+C` starts the config shortcut instead of quitting.

The interface is keyboard-only. Mouse capture is disabled; clicks and wheel
reports are ignored, including during confirmation dialogs.

## Scope

Commit lists include history reachable from the current HEAD and local
branches. Remote operations fetch refs and only fast-forward the current branch.
taid does not initialize submodules or manage nested submodules. See
[PLAN.md](PLAN.md) for the original development roadmap.

## License

MIT — see [LICENSE](LICENSE).
