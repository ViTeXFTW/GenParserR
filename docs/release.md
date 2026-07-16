# Release process

`dev` is the staging and default branch. `prod` contains live releases. Both
release types are started manually from the `Release` workflow on the `dev`
branch; tags created by the workflow do not trigger another workflow.

## Versioning

`[workspace.package].version` in `Cargo.toml` is the release version and must be
a plain `major.minor.patch` value. The workflow stamps the VS Code extension
with that version while packaging, so its committed manifest is not a release
input.

- A pre-release from version `1.0.5` gets a unique tag such as
  `v1.0.5-pre.42` on the tested `dev` commit.
- A live release gets `v1.0.5` on the `dev`-to-`prod` merge commit.

Tags are immutable. Do not reuse or move a release tag; bump the Cargo version
instead. GitHub pre-releases do not publish to the VS Code Marketplace. This
keeps the later live extension on the same Cargo version without conflicting
with Marketplace's numeric version rules.

The historical `v1.0.4` GitHub pre-release used the stable tag shape and also
published Marketplace version `1.0.4`. It remains as-is; `1.0.5` is the next
available live version.

## Before dispatching

Commit the version bump to `dev`, then run the normal local checks when
appropriate:

```sh
cargo test --locked
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings

cd editors/vscode
npm ci
npm test
cd ../..
```

Review the root README, extension README, and files under `docs/` whenever a
release changes installation, settings, supported platforms, or user-visible
behavior.

The release workflow repeats CI and the real-corpus gate before building any
release assets.

## Running a pre-release

1. Open **Actions → Release → Run workflow**.
2. Select the `dev` branch and `prerelease`.
3. Run the workflow.

The workflow pins the selected `dev` commit, runs all gates, builds Windows x64
and Linux x64 server archives and platform VSIX packages, then creates a GitHub
pre-release with checksums. It does not change `prod` or publish to Marketplace.

## Running a live release

1. Open **Actions → Release → Run workflow**.
2. Select the `dev` branch and `release`.
3. Run the workflow.

After all gates and packages succeed, the workflow creates a `dev` → `prod`
PR and merges it automatically. The merge is rejected if `dev` moved since the
workflow started. The merge commit receives the stable tag and GitHub Release;
the packaged extensions are then published to Marketplace when `VSCE_PAT` is
configured.

If `prod` contains changes that are not in `dev`, the workflow stops before
building and asks for `prod` to be merged back into `dev` first.

## After release

Verify that the GitHub Release contains both server archives, both
platform-specific `.vsix` files, and `SHA256SUMS.txt`. Install the matching
`.vsix` in a clean VS Code profile and open a Generals INI file. For a live
release, confirm the same version appears in the Marketplace when publishing is
enabled.

## Repository settings

- Keep `dev` as the default branch so the manual workflow is available.
- Keep normal pull-request and CI protection on `dev`.
- Under **Settings → Actions → General**, enable **Allow GitHub Actions to
  create and approve pull requests**. The workflow already requests only the
  required `contents: write` and `pull-requests: write` token scopes.
- Keep `prod` available for the workflow-created merge PR. If branch protection
  is later added to `prod`, give the release workflow a deliberate bypass or
  change the flow to wait for approval.
- Configure `VSCE_PAT` to publish live VS Code Marketplace builds. Without it,
  the stable GitHub Release still succeeds.
