# taid — Development Plan

> **taid** (from *test-aid*) — a self-contained terminal UI for managing which commits
> the main repository records for its submodules, browsing a repository's history
> locally, and switching branches.
>
> 100% local: it never fetches, never pushes, never creates remote refs, never rewrites
> history. All git operations go through a statically-linked libgit2 (`git2` crate) —
> the shipped binary never shells out to the `git` CLI and never touches the network.

- Binary name: `taid`
- Language: Rust
- UI: `ratatui` + `crossterm`
- Git: `git2` (vendored libgit2, no ssh/https features)

---

## 1. Product behavior (the contract)

`taid` is spawned in any directory from the terminal:

- **Not a git repository** → centered popup: *"This directory is not a git repository."*
  with an **OK** button (mouse click or `Enter`) that exits.
- **Git repo with submodules** → left pane lists the main repo + every submodule; the
  right pane shows the selected repo's commit history. The user picks, per submodule,
  the commit the **main repo** should record. **Save** applies all picks; quitting
  without Save changes nothing.
- **Git repo without submodules** → same UI restricted to the main repo: browse history
  and move HEAD to any past commit (a plain checkout — future commits are never lost).

Every commit row shows: **short hash · title · timestamp (date + hour, in the commit's
own timezone)**, with the full hash available in the detail/summary area.

Choosing an older commit for a submodule **does not delete or cancel** any commit —
newer commits remain in the submodule's history; only what the superproject records
changes (plus a checkout of that commit in the submodule's working directory).

## 2. Design decisions (veto now if any should change)

| # | Decision |
|---|---|
| D1 | **Save (submodule)** = checkout the chosen commit inside the submodule (detached HEAD) **and** stage the new pointer in the superproject (equivalent of `git add <submodule>`). `taid` never creates a commit in the main repo and never pushes. |
| D2 | **Save (main repo)** = plain `git checkout <sha>` (detached HEAD). No reset, no branch movement, future commits stay reachable. Checkout happens on **Save**, not on selection, for consistency with "no Save → nothing happens". |
| D3 | Commit lists show history reachable from the repo's/submodule's current local HEAD. No fetching. Uninitialized submodules are listed as *not initialized* and cannot be edited. |
| D4 | Branch switcher lists **local** branches only (v1). Remote-tracking branches are a possible later addition. |
| D5 | Repo discovery walks up from cwd (like git). The header always shows the discovered repo root. |
| D6 | The main repo is always the first entry of the left pane, so the superproject itself can also be time-traveled / branch-switched. |
| D7 | Linux release builds target **musl → fully static binaries** (zero runtime deps). macOS/Windows builds depend only on standard OS libraries. libgit2 is vendored into the binary (no system libgit2 needed). |
| D8 | Install location: `~/.local/bin` (override: `TAID_INSTALL_DIR`). License: MIT. |

## 3. Keymap (planned contract)

| Key | Action |
|---|---|
| `↑ ↓` / `j k` | move in list |
| `PgUp PgDn` / `Home End` | fast navigation |
| `Tab` / `Shift+Tab` | switch focus (repos ↔ commits) |
| `Enter` | choose / un-choose the commit under the cursor |
| `b` | branch switcher for the selected repo |
| `s` | save & apply pending picks |
| `r` | refresh git state |
| `?` | help popup |
| `q` / `Esc` | quit / close popup |

Mouse: click to focus panes, select rows, and press buttons (the OK button is clickable).

---

## 4. Steps

Each step ends with one or more commits, so every point is independently reviewable
and revertable. Tests may use the `git` CLI to build fixtures; the shipped binary
never does.

### Step 1 — Project scaffold ✅
- [x] `Cargo.toml` deps: `ratatui` 0.29, `crossterm` 0.28 (unified with ratatui's),
      `git2` 0.20 (default-features off, vendored libgit2, **no** ssh/https/openssl),
      `anyhow`, `chrono` (std only). Edition 2024, MSRV 1.85.
- [x] Verified with `cargo tree`: no `openssl-sys`, no `libssh2-sys`, single crossterm
- [x] Minimal `main.rs`: `--version` / `--help` handled, exit cleanly (no TUI yet)
- [x] README stub, LICENSE (MIT)
- [ ] `cargo build` + `cargo clippy -D warnings` clean
**Done when:** `./target/debug/taid --version` prints the version; build has zero warnings.

### Step 2 — Git core: read-only model ✅
- [x] `src/gitx.rs`: discover repo walking up from cwd (expose root path);
      `Ok(None)` cleanly means "not a git repository"
- [x] Repo introspection: current branch or detached sha (or unborn HEAD),
      clean/dirty flag (untracked files count, matches `git status`)
- [x] Submodule enumeration: name, path, recorded OID (HEAD tree) and staged OID
      (index), workdir OID, initialized?, clean/dirty, HEAD state, workdir path
- [x] Commit log: oid, git-style short sha, title, author, unix time + UTC
      offset, formatted `YYYY-MM-DD HH:MM` in the commit's own timezone;
      lazy materialization (`ensure_loaded`) with all oids collected up front
- [x] Fixture-based unit tests: real repos built with git2 only (no git CLI,
      no network); superproject fixture with matching / ahead / uninitialized
      submodules
- [x] CLI stub now reports discovery, HEAD, dirtiness, submodules and newest
      commit through the real model (exercises it end to end)
**Done when:** unit tests green for discovery, submodule enumeration, commit listing. ✅

### Step 3 — Git core: operations ✅
- [x] `src/gitops.rs`: `checkout_oid(repo, oid)` — SAFE checkout (never
      `--force`), detached HEAD; conflict/exists errors mapped to actionable
      messages ("commit or stash them first")
- [x] `set_submodule_pointer(super, path, oid)` — SAFE checkout inside the
      submodule (detached, like `git submodule update`) + stage the gitlink in
      the superproject's index (like `git add <submodule>`); clear error when
      the submodule is not initialized
- [x] List local branches alphabetical with current marked (`BranchEntry`);
      `switch_branch(repo, name)` — SAFE checkout + move HEAD to the branch ref
- [x] Restructured to lib + bin (`src/lib.rs`) so ops are exercisable before
      the TUI wires them up
- [x] Tests (8): older pointer keeps newer commits reachable (branch ref +
      revwalk intact); dirty tree refused with clear error and nothing changed
      (checked for checkout, submodule pointer, branch switch); branch switch
      moves HEAD and worktree, works from detached HEAD and as a no-op; unborn
      HEAD lists no branches
- [x] Cross-validated against real git on a CLI-created superproject: after
      `set_submodule_pointer`, `git status` shows `M  sub` (staged),
      `git diff --cached` shows a clean Subproject-commit transition, and the
      submodule's branch still points at the newest commit
**Done when:** all operation tests green; no history ever rewritten by any op. ✅

### Step 4 — TUI foundation ✅
- [x] `src/term.rs`: terminal init/teardown (alternate screen, raw mode,
      mouse capture) + panic hook that restores the terminal BEFORE the
      panic message prints; no-tty fails with a clear message
- [x] `src/event.rs`: event loop (keyboard + mouse, resize; Ctrl+C always quits)
- [x] `src/app.rs`: terminal-free state machine — modes (Normal/NotRepo/Error),
      focus cycling, ScrollState with paging/clamping/ensure_visible,
      popups, hit-zone routing for clicks
- [x] `src/ui.rs`: base layout — header (repo root, branch/detached, dirty
      flag), left repo list (main + submodules with state), right pane
      placeholder, footer keymap; `?` help popup; not-repo/error popup with
      clickable [ OK ] button; renderer registers hit zones each frame
- [x] Hardening: zero-sized and tiny terminals cannot panic (0×0 ptys found
      via testing; regression-tested with TestBackend)
- [x] `tests/pty.rs`: real pty integration tests (portable-pty) driving the
      actual binary in an 80×24 pty with timeouts and output assertions:
      q quits, OK popup + Enter exits, panic restores before printing.
      Replaced the earlier script/printf approach (fragile, 0×0 ptys, could
      leak mouse-tracking into the host terminal when interrupted)
- [x] Found & fixed via pty testing: input sent before raw mode is on gets
      CR→NL-translated (harness waits for the first frame before sending)
**Done when:** runs correctly in both a repo and a non-repo dir; terminal restored
after exit and after an induced panic. ✅ (36 unit tests + 3 pty tests)

### Step 5 — Commit browsing UX ✅
- [x] Left pane: main repo first; per entry: name, recorded sha, `≠ workdir`
      drift marker, `✓` pending-pick badge, dirty marker, `not initialized` badge
- [x] Right pane: 2-line summary (identity · HEAD · recorded/current vs workdir ·
      clean/dirty · pick line) + commit list rows `marker short-sha date+time title`
      with `●` = recorded/current, `○` = submodule's checkout, `✓` = pending pick
- [x] Scrolling (arrows/vi keys/PgUp/PgDn/Home/End) + mouse wheel; `Enter` toggles
      the pick (picking the current commit is a no-op); click selects rows
- [x] Adaptive single-pane layout when the repo has no submodules (focus locked
      to the commit list)
- [x] `r` refresh — rebuilds state from disk, picks survive (keyed by main/sub
      path), histories reload lazily; lazy materialization: 60 rows on open, +40
      ahead of the viewport while scrolling (verified with a 200-commit repo)
- [x] Histories walk HEAD **plus all local branches** so newer commits stay
      visible/selectable even after moving HEAD back (never a dead end)
- [x] Bugs found by tests & fixed: commit cursor moved before the entry's history
      loaded (first Down landed on row 0); clicking a commit row hit the pane
      zone first (hit-test now picks the smallest containing region)
**Done when:** listing matches `git log --oneline` ground truth in a fixture repo;
markers and layout correct. ✅ (50 unit + 4 pty tests)

### Step 6 — Branch switcher
- [ ] `b` opens a popup for the repo selected in the left pane
- [ ] List local branches (current one marked; detached-HEAD state shown)
- [ ] `Enter` switches branch, `Esc` cancels; state refreshes everywhere
- [ ] Footnote in the popup when used on a submodule: *switching a submodule's branch
      does not change what the main repo records*
**Done when:** branch switch verified on main repo and submodule fixtures; UI refreshes.

### Step 7 — Save flow
- [ ] Pending-picks model (per repo: chosen OID vs recorded)
- [ ] `s` → confirmation popup: planned changes as `repo: <old sha> → <new sha> — <title>`,
      plus warnings for dirty working trees
- [ ] `Enter` applies best-effort per item, collecting per-item errors; result popup
      shows successes/failures; `Esc` cancels (nothing done)
- [ ] After apply: refresh all state from disk
- [ ] `dev/fixtures.sh`: script that builds a test superproject with 2 submodules and
      multi-commit histories for manual end-to-end testing
**Done when (manual E2E):** pick an old commit on a submodule, Save → superproject
shows the staged pointer change and the submodule workdir is at the old commit, while
`git log` in the submodule still lists the newer commits; quitting without Save leaves
everything untouched.

### Step 8 — Polish & hardening
- [ ] Mouse: click rows to select/focus, click buttons
- [ ] Truncate long titles/paths with ellipsis; consistent color scheme
- [ ] Optional: `/` filter in the commit list
- [ ] README complete: usage, keymap, install, build-from-source
- [ ] `cargo fmt` + `cargo clippy -D warnings` clean
**Done when:** clippy clean, README complete, mouse/resize manually verified.

### Step 9 — install.sh
- [ ] Detect OS/arch via `uname` → target triple + archive extension (mapping table below)
- [ ] Modes: default = download latest release (`--tag vX.Y.Z` to pin,
      `TAID_REPO=owner/name` to override); `--bin <path>` = offline install of a local
      binary; `--dry-run`
- [ ] Download with curl (fallback wget), verify against `SHA256SUMS.txt`
- [ ] Install to `~/.local/bin` (or `TAID_INSTALL_DIR`), make executable
- [ ] PATH: if missing, append a marked `export PATH=` line to the right shell rc
      (zsh → `~/.zshrc`; bash → `~/.bashrc`, or `~/.bash_profile` on macOS; fallback
      `~/.profile`), idempotently; print next-step instructions
**Done when:** `./install.sh --bin target/debug/taid` works locally (new shell finds
`taid` on PATH); `--dry-run` prints the correct download URL for every platform.

### Step 10 — CI + Release workflows + v0.1.0
- [ ] `.github/workflows/ci.yml`: fmt + clippy + test on push/PR (ubuntu-latest)
- [ ] `.github/workflows/release.yml` on tag `v*`: build matrix (table below), package
      `taid-<tag>-<target>.tar.gz|.zip` (containing `taid[.exe]`, README, LICENSE),
      aggregate `SHA256SUMS.txt`, create the GitHub Release
- [ ] Tag `v0.1.0` and publish
- [ ] Smoke-test: download artifacts, run `--version`; verify `install.sh` download
      mode against the real release
**Done when:** the release page lists all artifacts + checksums and `install.sh`
installs from it.

> Prerequisite for step 10: the repo must be pushed to GitHub. Create the remote repo
> and tell me the `owner/name` slug (or grant me `gh` access) — I'll wire the remote.
> Steps 1–9 work fully offline.

## 5. Release matrix

| Target triple | Runner | Toolchain | Archive |
|---|---|---|---|
| `x86_64-unknown-linux-musl` | ubuntu-latest | cargo-zigbuild (fully static) | `.tar.gz` |
| `aarch64-unknown-linux-musl` | ubuntu-latest | cargo-zigbuild (fully static) | `.tar.gz` |
| `armv7-unknown-linux-musleabihf` | ubuntu-latest | cargo-zigbuild | `.tar.gz` |
| `x86_64-apple-darwin` | macos-latest | rustup target add (Xcode cross) | `.tar.gz` |
| `aarch64-apple-darwin` | macos-latest | native | `.tar.gz` |
| `x86_64-pc-windows-msvc` | windows-latest | native | `.zip` |
| `aarch64-pc-windows-msvc` | windows-latest | rustup target add (MSVC ARM64) | `.zip` |

`install.sh` platform mapping:

| `uname -s` | `uname -m` | Target |
|---|---|---|
| Linux | x86_64 | `x86_64-unknown-linux-musl` |
| Linux | aarch64 / arm64 | `aarch64-unknown-linux-musl` |
| Linux | armv7l / armv8l | `armv7-unknown-linux-musleabihf` |
| Darwin | x86_64 | `x86_64-apple-darwin` |
| Darwin | arm64 | `aarch64-apple-darwin` |
| MINGW*/MSYS*/CYGWIN* or Windows | x86_64 / AMD64 | `x86_64-pc-windows-msvc` |
| MINGW*/MSYS*/CYGWIN* or Windows | arm64 / ARM64 | `aarch64-pc-windows-msvc` |

## 6. Out of scope (v1; candidates for v2)

Network operations (fetch/push/pull), creating commits in the superproject, submodule
add/remove/init/clone, nested submodules (only first-level submodules are listed),
remote-branch checkout, git worktrees, configurable themes.
