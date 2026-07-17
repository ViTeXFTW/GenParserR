# ZeroSyntax v2 for VS Code

ZeroSyntax adds diagnostics, completion, navigation, refactoring, semantic
highlighting, and quick fixes for *Command & Conquer: Generals – Zero Hour* INI
files. The platform-specific extension package includes the language server, so
there is nothing else to install.

## Install

1. Download the `.vsix` for Windows x64 or Linux x64 from the
   [latest release](https://github.com/ViTeXFTW/ZeroSyntaxV2/releases/latest).
2. In VS Code, open **Extensions**, choose **Views and More Actions …**, then
   select **Install from VSIX…**.
3. Open the downloaded file and reload VS Code if prompted.
4. Open your map or mod folder and start editing an `.ini` file.

You can also install from a terminal:

```sh
code --install-extension zerosyntax-vscode-<platform>-<version>.vsix
```

## Recommended setup

Open the folder containing your project rather than a single file. ZeroSyntax
indexes the workspace so definitions, references, rename, and completion work
across its INI files.

When editing `map.ini` or `solo.ini`, configure **ZeroSyntax v2: Base Ini Roots**
with any game or mod folders and `.big` archives that load before the map. This
prevents false unresolved-reference warnings and enables W3D model and bone
checks.

## Settings

| Setting | Default | Purpose |
| --- | --- | --- |
| `zerosyntax.baseIniRoots` | `[]` | Base game/mod directories and `.big` archives used for map and model checks. |
| `zerosyntax.schema.path` | empty | Custom schema JSON; invalid files fall back to the built-in schema. |
| `zerosyntax.format.enable` | `false` | Enables indentation formatting. Changing it restarts the server. |
| `zerosyntax.server.path` | empty | Uses a custom `zerosyntax-lsp` binary instead of the bundled one. |
| `zerosyntax.trace.server` | `off` | Logs LSP traffic for troubleshooting. |

Formatting is intentionally off by default. Enable it only when you want
**Format Document** or format-on-save to normalize indentation.

## INI file association

The extension associates `.ini` files with **Generals INI**. If your workspace
also contains unrelated INI files, keep those as plain text and override the
association only for the folders you want ZeroSyntax to handle:

```json
{
  "files.associations": {
    "**/*.ini": "plaintext",
    "Data/INI/**/*.ini": "generals-ini",
    "Maps/**/*.ini": "generals-ini"
  }
}
```

Use the language selector in VS Code's status bar to change an individual file.

## Troubleshooting

- If the server cannot be found, reinstall the `.vsix` for your platform. A
  source checkout does not contain a bundled server until it is built.
- If map references are reported as missing, configure
  `zerosyntax.baseIniRoots` and check that the paths point to the required INI
  folders or `.big` archives.
- For detailed logs, set `zerosyntax.trace.server` to `messages` or `verbose`,
  reproduce the problem, then open **Output → ZeroSyntax v2 Language Server**.

Report reproducible problems through
[GitHub Issues](https://github.com/ViTeXFTW/ZeroSyntaxV2/issues) with a minimal
INI sample, your OS, VS Code version, and ZeroSyntax version.

Contributor build and packaging instructions are in the
[VS Code development guide](https://github.com/ViTeXFTW/ZeroSyntaxV2/blob/dev/docs/vscode-development.md).
