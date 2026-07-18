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

To debug the extension, open `editors/vscode` in VS Code and press <kbd>F5</kbd>.
The repository's launch configuration points the Extension Development Host at
the locally built debug server.

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
