# rv — local code reviews in Git

`rv` stores append-only review threads under `refs/reviews/<branch>`. Reviewed commits remain reachable even after jj rewrites. The Rust CLI lives in `cli/`; the Lua Neovim plugin lives in `nvim/`.

The format and command contracts are in [`design/storage_design.md`](design/storage_design.md) and [`design/cli_design.md`](design/cli_design.md).

## Build

Requires Rust/Cargo and Git. jj support requires a Git-colocated jj repository.

```sh
cargo build --release --manifest-path cli/Cargo.toml
cargo install --path cli/rv
```

On Nix, a temporary toolchain can be used without changing your global environment:

```sh
nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#rustfmt nixpkgs#stdenv.cc
```

## CLI example

Every branch operation requires an explicit `-b`; creating a branch requires `--create`. The CLI never prompts or opens an editor.

```sh
# Choose the precise code snapshot you reviewed.
commit=$(git rev-parse HEAD)
printf '{"type":"comment","commit":"%s","body":"Please add a regression test."}\n' "$commit" |
  rv commit -b task --create --reviewed "$commit" --json

rv show -b task --json
rv show -b task --at HEAD --path src/main.rs --json  # plain Git
rv show -b task --at @ --json                       # colocated jj
rv log -b task --json
rv check -b task --json
rv branches --json
rv id --json
```

Use `--json` in integrations: successful commands write one versioned JSON object to stdout; errors write one to stderr and return a stable nonzero exit code. `rv check` returns exit code 0 even when `ok` is false. Review branch updates use compare-and-swap; no code checkout, index, or normal branch is changed.

## Neovim

Add `nvim/` to your runtime path (or configure your plugin manager to use that subdirectory), put `rv` on `PATH`, then:

```lua
require('rv').setup({
  -- Optional explicit branch; there is no implicit default.
  branch = 'task',
  -- Save the SOURCE buffer before composing a comment. Does not commit reviews.
  autosave_on_comment = false,
})
```

See [`nvim/README.md`](nvim/README.md) for commands, mappings, draft management and diffview integration. Review drafts stay in memory until explicit save calls `rv commit`; do not expect unsaved drafts to survive exiting Neovim.

Normal-buffer comments pin a saved jj `@` snapshot. Diffview and diffview-plus comments pin the displayed new-side commit, not the current working copy. Old-side/deleted-line anchors are intentionally unsupported by the v1 format.

## Test

```sh
cargo fmt --manifest-path cli/Cargo.toml --all -- --check
cargo test --manifest-path cli/Cargo.toml
nvim/tests/run-headless.sh
RV_CLI="$PWD/cli/target/debug/rv" nvim/tests/real-cli-smoke.sh
```

Tests use throwaway repositories and real Git/jj. Editor tests cover both Diffview adapters with upstream-shape fixtures; they do not replace live UI compatibility testing against future upstream versions.
