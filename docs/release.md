# Release process

`dev` is the staging branch. `prod` is the live branch. Merging `dev` into
`prod` creates the `vX.Y.Z` tag from the Rust workspace version in `Cargo.toml`
and runs the live release pipeline.

## Prerequisites

- Push access to `dev` and `prod`.
- A clean checkout of `dev` for staging work.
- Repository secret `VSCE_PAT` to publish VS Code Marketplace builds. Without
  it, the live workflow still creates the GitHub Release and `.vsix` assets.

## Before merging

```sh
cargo test --locked
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings

cd editors/vscode
npm ci
npm run compile
cd ../..
```

Run the corpus gate when the ignored corpus is available:

```sh
scripts/fetch-corpus.sh
cargo test --release -p zerosyntax-analysis --test corpus -- --ignored --nocapture
```

On Windows, use `pwsh scripts/fetch-corpus.ps1`.

## Version bump

1. Update `[workspace.package].version` in `Cargo.toml`.
2. Update `editors/vscode/package.json` if preparing a local package. The
   staging and live workflows stamp the extension version during packaging.
3. Run `cargo check --locked` if `Cargo.lock` changes are expected.
4. Commit the version bump before merging `dev` into `prod`.

## Staging

Pushes to `dev` run CI. After CI passes on `dev`, `Dev Pre-release` publishes
Marketplace pre-release builds only when `[workspace.package].version` changed.
The check compares the final commit against the branch state before the push, so
batched pushes with a version bump still publish. Dependabot-only dependency
bumps therefore run CI without publishing a staging extension. Use
`workflow_dispatch` for a manual pre-release rerun.

## Live release

Open a PR from `dev` into `prod`. After merge, `Prod Release Tag` reads
`Cargo.toml`, creates `vX.Y.Z` if it does not already exist, and calls the live
`Release` workflow.

The live `Release` workflow will:

- Re-run CI.
- Verify the tag matches the Rust workspace version.
- Build `zerosyntax-lsp` for Windows x64 and Linux x64.
- Package platform-specific VS Code `.vsix` files.
- Create a GitHub Release with checksums.
- Publish to the VS Code Marketplace only when `VSCE_PAT` is configured.

## GitHub repository settings

Set `dev` as the default branch unless there is a reason to keep another branch
as the default.

Protect `dev` with:

- Require pull requests before merging.
- Require status checks from CI.
- Block force pushes and direct pushes.

Protect `prod` with:

- Require pull requests before merging.
- Require one maintainer approval.
- Require status checks from CI.
- Require branches to be up to date before merging.
- Block force pushes and direct pushes.
