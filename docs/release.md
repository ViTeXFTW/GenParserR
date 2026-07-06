# Release process

Releases are tag-driven. The tag version must match the Rust workspace version
in `Cargo.toml`.

## Prerequisites

- Push access to `main`.
- A clean checkout of `main`.
- Optional: repository secret `VSCE_PAT` to publish VS Code Marketplace builds.
  Without it, the workflow still creates the GitHub Release and `.vsix` assets.

## Before tagging

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
   release workflow stamps the extension version from the tag during packaging.
3. Run `cargo check --locked` if `Cargo.lock` changes are expected.
4. Commit the version bump.

## Tag and publish

```sh
git tag vX.Y.Z
git push origin main
git push origin vX.Y.Z
```

The `Release` workflow will:

- Re-run CI.
- Verify the tag matches the Rust workspace version.
- Build `zerosyntax-lsp` for Windows x64 and Linux x64.
- Package platform-specific VS Code `.vsix` files.
- Create a GitHub Release with checksums.
- Publish to the VS Code Marketplace only when `VSCE_PAT` is configured.

## GitHub repository settings

Protect `main` with:

- Require pull requests before merging.
- Require one maintainer approval.
- Require status checks from CI.
- Require branches to be up to date before merging.
- Block force pushes and direct pushes.
