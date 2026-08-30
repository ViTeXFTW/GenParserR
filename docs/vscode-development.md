# VS Code extension development

These instructions are for contributors. Users should install a platform
package from [GitHub Releases](https://github.com/ViTeXFTW/ZeroSyntaxV2/releases/latest).

## Build and test

From the repository root:

```sh
cargo build --locked -p zerosyntax-server

cd editors/vscode
npm ci
npm test
```

## Fast local loop (F5)

Open the **repository root** in VS Code and press <kbd>F5</kbd> (the
_Run Extension (local dev server)_ configuration in `.vscode/launch.json`).
This runs one build task that, in parallel:

1. builds the debug server (`cargo build -p zerosyntax-server`), and
2. bundles the extension with source maps (`npm run compile:dev` in
   `editors/vscode`).

It then opens an Extension Development Host window with `ZEROSYNTAX_LSP_PATH`
pointed at `target/debug/zerosyntax-lsp[.exe]`, so the extension loads the
freshly built server without copying a binary or installing a `.vsix`.

- **Changed the server?** Rebuild and reload: run the build task (or press
  <kbd>F5</kbd> again), then restart the language server from the dev-host
  window (Command Palette → _Developer: Reload Window_).
- **Changed the extension (TypeScript)?** _Developer: Reload Window_ in the
  dev-host picks up the rebuilt bundle; breakpoints work via the emitted source
  maps.

The first run needs `npm ci` in `editors/vscode` so the build task can find the
TypeScript/esbuild toolchain.

For a lighter, server-only loop (no extension), see the Neovim harness in
[`editors/nvim/README.md`](../editors/nvim/README.md).

## Choose a server binary

The extension resolves the server in this order:

1. `zerosyntax.server.path`.
2. The `ZEROSYNTAX_LSP_PATH` environment variable used by development tooling.
3. `editors/vscode/server/zerosyntax-lsp[.exe]`.
4. `zerosyntax-lsp` on `PATH`.

## Package locally

Build the release server, copy it into `editors/vscode/server`, then package the
extension:

```sh
cargo build --locked --release -p zerosyntax-server
cd editors/vscode
npm ci
npm run package
```

The release workflow performs the platform-specific binary staging and VSIX
target selection automatically. See the [release process](release.md).
