# Diagnostics and quick fixes

ZeroSyntax reports stable diagnostic codes. Use the code shown by your editor
when searching for an issue or suppressing an intentional warning.

## Suppress a diagnostic in one file

Add a file-scope comment before the first INI block:

```ini
; zerosyntax-disable: unresolved-reference, unreachable-set
```

Separate multiple codes with spaces or commas. Multiple file-scope pragma lines
accumulate. A misspelled code produces `unknown-suppression` instead of silently
hiding nothing.

Suppressions are intended for warnings and hints that are valid for a specific
file. Fix error-level syntax and schema problems rather than suppressing them.

## Diagnostic codes

| Code | Meaning |
| --- | --- |
| `syntax` | The file cannot be parsed cleanly, such as when an `End` is missing. |
| `stray-field` | A field appears outside a valid block or module. |
| `unknown-block` | A top-level block is not in the Generals INI schema. |
| `overrides` | A map layer redefines an existing object-style definition. |
| `duplicate-definition` | The same definition is declared more than once. |
| `map-forward-reference` | A map or solo INI references a definition declared later in that file, after the point where the game needs it. |
| `map-projectile-object` | A weapon uses a map-defined projectile object that the game cannot resolve reliably. |
| `unreachable-set` | A `WeaponSet` or `ArmorSet` cannot activate, or an upgrade module has no matching set. |
| `unknown-field` | A field is not valid in the current block or module. |
| `missing-module-tag` | A module is missing its required `ModuleTag_*` name. |
| `unknown-module` | A module type is not known for the current module slot. |
| `missing-condition` | A conditional state block is missing its condition token. |
| `missing-value` | A field requires a value but none was provided. |
| `bad-bool` | A boolean is not `Yes` or `No`. |
| `non-positive` | A value that must be greater than zero is not. |
| `bad-percent` | A percentage is malformed or outside its allowed range. |
| `bad-color` | A color value is malformed. |
| `bad-coord` | A coordinate value is malformed. |
| `bad-number` | A numeric field does not contain a valid number. |
| `bad-enum` | A value is not a member of the expected enum. |
| `bad-flag` | A bitflag is not a member of the expected flag set. |
| `bad-prefixed` | A tagged value does not use its required `Prefix:value` form. |
| `unresolved-reference` | A referenced definition is not found in the workspace or configured base INI roots. |
| `unknown-model` | A model name is not found in the indexed W3D assets. |
| `unknown-model-member` | A bone or subobject is not found in the models active in that scope. |
| `unknown-suppression` | A `zerosyntax-disable` comment names an unknown code. |
| `module-wrong-slot` | A module type is used under the wrong slot. |
| `duplicate-module-tag` | Two modules in one object use the same module tag. |
| `editor-default-module` | A placeholder module value should be replaced before shipping. |

## Quick fixes

Open your editor's lightbulb or code-action menu on a diagnostic to see the
available fix.

| Quick fix | When it appears |
| --- | --- |
| Insert missing `End` | A block or module is unterminated. |
| Replace with a known value | An enum or bitflag is close to a valid value. |
| Create a stub definition | A reference points to a missing definition that can be scaffolded safely. |
| Remove an unreachable `WeaponSet` or `ArmorSet` | An upgrade-conditioned set can never activate. |
| Insert a matching upgrade module or set | An object has only one side of an upgrade-conditioned weapon or armor setup. |
| Suppress a code in this file | A warning or hint is intentional for the current file. |
