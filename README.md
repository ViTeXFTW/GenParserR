# ZeroSyntax v2

<p align="center">
  <img src="icon/ZeroSyntaxLogo256.png" alt="ZeroSyntax logo">
</p>

[![CI](https://github.com/ViTeXFTW/ZeroSyntaxV2/actions/workflows/ci.yml/badge.svg)](https://github.com/ViTeXFTW/ZeroSyntaxV2/actions/workflows/ci.yml)
[![Release](https://github.com/ViTeXFTW/ZeroSyntaxV2/actions/workflows/release.yml/badge.svg)](https://github.com/ViTeXFTW/ZeroSyntaxV2/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

ZeroSyntax v2 brings modern editor support to the INI scripting files used by
*Command & Conquer: Generals – Zero Hour*. It helps modders and map authors find
mistakes early and navigate large game or mod workspaces.

The project includes the IDE-independent `zerosyntax-lsp` language server and a
VS Code extension with the server bundled.

## What you get

- Schema-aware diagnostics and quick fixes for blocks, fields, modules, values,
  references, module tags, and missing `End` statements.
- Context-aware completion for INI keywords, enum values, flags, and definitions
  in your workspace.
- Hover information, go to definition, find references, rename, workspace and
  document symbols, and folding.
- Semantic highlighting for Generals INI files.
- Optional indentation formatting, disabled by default so existing files are
  never reformatted without your consent.
- Base-game and mod indexing for `map.ini` and `solo.ini`, including W3D model
  and bone checks.

## Install the VS Code extension

ZeroSyntax supports Windows x64 and Linux x64 release builds.

1. Download the `.vsix` for your platform from the
   [latest GitHub release](https://github.com/ViTeXFTW/ZeroSyntaxV2/releases/latest).
2. In VS Code, open **Extensions**, choose **Views and More Actions …**,
   select **Install from VSIX…**, and open the downloaded file.
3. Open your mod or map folder, then open an `.ini` file.

The extension treats `.ini` files as **Generals INI**. If a workspace also
contains unrelated INI files, use VS Code's `files.associations` setting to
limit that language association to the appropriate folders.

See the [VS Code extension guide](editors/vscode/README.md) for settings and
troubleshooting.

## Use the standalone language server

Download the `zerosyntax-lsp` archive for your platform from the
[latest release](https://github.com/ViTeXFTW/ZeroSyntaxV2/releases/latest),
extract it, and configure your editor to run the binary over stdio.

See the [language server guide](docs/language-server.md) for initialization
options and editor integration details.

## Check files from the command line

The same binary can run diagnostics without an editor:

```sh
zerosyntax-lsp check Data/INI
zerosyntax-lsp check map.ini --base-root "C:/Games/Zero Hour"
zerosyntax-lsp check --json --stdin-filename map.ini - < generated.ini
```

This is intended for CI, pre-commit checks, and LLM edit/check loops. Errors
produce exit code 1 by default; add `--fail-on warning` for a stricter gate.
See the [standalone guide](docs/language-server.md#command-line-diagnostics) for
the complete output and exit-code contract.

## Configure map and model checks

For complete `map.ini` and `solo.ini` diagnostics, set
`zerosyntax.baseIniRoots` in VS Code to the base game or mod directories and/or
`.big` archives that load before the map. The same setting also enables W3D
model and bone completion and validation.

`zerosyntax.analysis.modelMemberStrictness` controls bone/subobject warnings:
`off`, `compatible` (the default; present in any applicable model), or `strict`
(present in every applicable model).

```json
{
  "zerosyntax.baseIniRoots": [
    "C:/Games/Zero Hour",
    "C:/Mods/MyMod/Data/INI",
    "C:/Mods/MyMod.big"
  ]
}
```

## Documentation

- [Diagnostics, suppression, and quick fixes](docs/diagnostics.md)
- [Standalone language server setup](docs/language-server.md)
- [Contributing](CONTRIBUTING.md)
- [Support](SUPPORT.md)
- [Security policy](SECURITY.md)

## License and trademarks

ZeroSyntax v2 is available under the [MIT License](LICENSE).

ZeroSyntax v2 is an unofficial community project and is not affiliated with,
endorsed by, or sponsored by Electronic Arts. Command & Conquer and related
names are trademarks of their respective owners.
