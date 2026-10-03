# taid

**taid** is a local-first terminal UI for managing which commits your repository
records for its submodules — plus a quick way to time-travel through a repo's own
history and switch branches, without ever touching the network.

> **Status: work in progress.** See [PLAN.md](PLAN.md) for the roadmap and current
> step. This README will grow as steps land.

## What it will do

Spawn `taid` in any directory:

- **Not a git repo** → a popup tells you so, with an OK button.
- **Git repo, no submodules** → browse history and check out any past commit
  (future commits are never lost), switch branches.
- **Git repo with submodules** → for each submodule, pick the commit the main
  repository should record — the commit list shows hash, title and timestamp.
  **Save** applies all picks; quitting without saving changes nothing.

Everything stays local: no fetches, no pushes, no history rewrites. Choosing an
older commit for a submodule only changes what the superproject records; the
submodule's newer commits are untouched.

## Install

Coming in step 9: `install.sh` + prebuilt, dependency-free binaries for every
major platform (static musl builds on Linux).

## Build from source

```sh
cargo build --release
```

Requires only a Rust toolchain — libgit2 is compiled into the binary, so there
are no system dependencies to install.

## License

MIT — see [LICENSE](LICENSE).
