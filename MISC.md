Draw, Body, Behavior and Client will suggst all module types instead of only the allowed ones. Additionally there are no diagnostics for adding a wrong module type a Draw, body, behavior or client field/block.

When accepting a suggestion for a block, automatically add an 'End' keyword. Needs investigation for blocks which require addtional information like outer-blocks, modules blocks and conditionstate blocks, etc. on how to handle this in a nice way.

When just inside of a Draw block the suggestions will still suggest module types instead of the fields in the module. (Same for Body, Behavior and Client)

If an object has multiple modules with the same ModuleTag it should be highlighted so the user can update it, as this will make it so they can't remove them later.

Unless the user requests completions when in the outer scope, blocks will not be suggested like FXList, ObjectCreationList

Still missing completions for FXLists, 