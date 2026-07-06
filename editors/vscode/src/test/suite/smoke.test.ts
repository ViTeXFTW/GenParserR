import * as assert from "assert";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";

suite("ZeroSyntax VS Code extension", () => {
  test("activates the server and returns diagnostics and completions", async () => {
    const serverPath = process.env.ZEROSYNTAX_LSP_PATH;
      assert.ok(serverPath, "ZEROSYNTAX_LSP_PATH must point at ZeroSyntax-lsp");
    assert.ok(fs.existsSync(serverPath), `${serverPath} does not exist`);

    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "zerosyntax-vscode-"));
    const uri = vscode.Uri.file(path.join(dir, "Smoke.ini"));
    await vscode.workspace.fs.writeFile(
      uri,
      Buffer.from("Weapon SmokeGun\n  ScaleWeaponSpeed = Maybe\n  \nEnd\n")
    );

    const document = await vscode.workspace.openTextDocument(uri);
    await vscode.languages.setTextDocumentLanguage(document, "generals-ini");
    await vscode.window.showTextDocument(document);

    const extension = vscode.extensions.getExtension("ViTeXFTW.zerosyntax-vscode");
    assert.ok(extension, "zerosyntax-vscode extension not loaded");
    await extension.activate();

    const diagnostics = await waitFor(
      () => vscode.languages.getDiagnostics(uri),
      (items) => items.some((diag) => diag.code === "bad-bool"),
      "bad-bool diagnostic"
    );
    assert.ok(
      diagnostics.some((diag) => diag.severity === vscode.DiagnosticSeverity.Error),
      "expected an error diagnostic"
    );

    const completions =
      await vscode.commands.executeCommand<vscode.CompletionList>(
        "vscode.executeCompletionItemProvider",
        uri,
        new vscode.Position(2, 2)
      );
    const labels = completions.items.map((item) => String(item.label));
    assert.ok(
      labels.includes("PrimaryDamage"),
      `expected PrimaryDamage completion, got ${labels.slice(0, 10).join(", ")}`
    );
  });
});

async function waitFor<T>(
  get: () => T,
  done: (value: T) => boolean,
  label: string
): Promise<T> {
  const deadline = Date.now() + 15000;
  let value = get();
  while (Date.now() < deadline) {
    if (done(value)) {
      return value;
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
    value = get();
  }
  assert.fail(`timed out waiting for ${label}`);
}
