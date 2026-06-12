# Cutting a release

The release pipeline (`.github/workflows/release.yml`) is tag-triggered and
fully reproducible from a tag: it builds the server for every supported
platform, packages platform-specific VS Code extensions with the binary
bundled, and publishes a GitHub Release with checksums. Marketplace / Open VSX
publishing happens automatically when the tokens are configured (see below),
and is skipped — never failed — when they are not.

## Supported platforms

| Rust target | Release archive | `.vsix` target |
|---|---|---|
| `x86_64-pc-windows-msvc` | `genparser-lsp-vX.Y.Z-….zip` | `win32-x64` |
| `x86_64-unknown-linux-musl` (static) | `….tar.gz` | `linux-x64` |
| `x86_64-apple-darwin` | `….tar.gz` | `darwin-x64` |
| `aarch64-apple-darwin` | `….tar.gz` | `darwin-arm64` |

The Linux binary is musl-static (all dependencies are pure Rust), so one
binary covers every x64 distro regardless of glibc version.

## Procedure

1. **Run the local gates** that CI cannot (they need the fetched game-data
   corpus):

   ```sh
   cargo test
   scripts/fetch-corpus.sh        # once
   cargo test --release -p genparser-analysis --test corpus -- --ignored --nocapture
   ```

   The corpus run must show zero panics / `syntax` / `unknown-block` and no
   new false positives.

2. **Bump the version** in the *workspace* `Cargo.toml`
   (`[workspace.package] version`). The extension's `package.json` version is
   stamped from the tag at package time — no manual bump needed there.

3. **Tag and push.** The tag must be `v` + the workspace version; the
   `check-version` job fails the pipeline on a mismatch.

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

4. The workflow then:
   - builds `genparser-lsp` for the four targets above;
   - archives each (archives are created on the build machine so the Unix
     executable bit survives) with a per-file `.sha256`;
   - packages one `.vsix` per platform via `vsce package --target`, with the
     binary under the extension's `server/` directory;
   - creates the GitHub Release with all archives, all `.vsix` files, and an
     aggregate `SHA256SUMS.txt`, with auto-generated notes;
   - publishes the `.vsix` files to the VS Code Marketplace and Open VSX if
     the corresponding token secret is present.

## Marketplace credentials (one-time setup)

Publishing is keyed on two optional repository secrets:

- **`VSCE_PAT`** — a [VS Code Marketplace personal access token]
  (https://code.visualstudio.com/api/working-with-extensions/publishing-extension)
  for the `genparser` publisher (create the publisher at
  https://marketplace.visualstudio.com/manage first; the Azure DevOps PAT
  needs the *Marketplace → Manage* scope).
- **`OVSX_PAT`** — an [Open VSX access token](https://open-vsx.org/user-settings/tokens)
  for the same namespace (`npx ovsx create-namespace genparser` once).

GPL-3.0-or-later is compatible with both registries. Without the secrets the
release still ships the `.vsix` files as GitHub Release assets, installable
via `code --install-extension <file>.vsix`.

## Verifying a release (exit criteria)

On a clean machine with no Rust toolchain:

1. Install the extension from the Marketplace (or
   `code --install-extension genparser-vscode-<platform>-X.Y.Z.vsix`).
2. Open a Zero Hour `.ini` file — diagnostics, completion, and semantic
   highlighting must work with no further setup (the bundled binary is used;
   `genparser.server.path` still overrides it).
3. For the standalone archives: `sha256sum -c` against `SHA256SUMS.txt`, then
   point any LSP-capable editor at the unpacked `genparser-lsp` (README has
   Neovim / Helix / Zed blocks).
