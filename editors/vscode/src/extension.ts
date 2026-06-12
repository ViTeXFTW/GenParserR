import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

export function activate(context: vscode.ExtensionContext) {
  const serverPath = resolveServerPath(context);
  if (!serverPath) {
    vscode.window.showErrorMessage(
      "genparser: could not find the language server binary. Set `genparser.server.path` " +
        "or place `genparser-lsp` under the extension's `server/` directory or on your PATH."
    );
    return;
  }

  const serverOptions: ServerOptions = {
    run: { command: serverPath, transport: TransportKind.stdio },
    debug: { command: serverPath, transport: TransportKind.stdio },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "generals-ini" }],
    synchronize: {
      // Re-index when any .ini in the workspace changes on disk.
      fileEvents: vscode.workspace.createFileSystemWatcher("**/*.ini"),
    },
  };

  client = new LanguageClient(
    "genparser",
    "Generals INI Language Server",
    serverOptions,
    clientOptions
  );

  client.start();
  context.subscriptions.push({
    dispose: () => {
      void client?.stop();
    },
  });
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}

/** Resolve the server binary: explicit setting > env override > bundled > PATH. */
function resolveServerPath(context: vscode.ExtensionContext): string | undefined {
  const configured = vscode.workspace
    .getConfiguration("genparser")
    .get<string>("server.path");
  if (configured && configured.trim().length > 0) {
    return configured;
  }

  // Set by the repo's launch.json so the Extension Development Host picks up
  // the locally built debug binary without copying it into the extension.
  const fromEnv = process.env.GENPARSER_LSP_PATH;
  if (fromEnv && fs.existsSync(fromEnv)) {
    return fromEnv;
  }

  const exe = process.platform === "win32" ? "genparser-lsp.exe" : "genparser-lsp";
  const bundled = context.asAbsolutePath(path.join("server", exe));
  if (fs.existsSync(bundled)) {
    if (process.platform !== "win32") {
      // The zip-based .vsix install can drop the executable bit.
      try {
        fs.chmodSync(bundled, 0o755);
      } catch {
        // Read-only extension dir: either the bit survived or spawn fails loudly.
      }
    }
    return bundled;
  }

  // Fall back to PATH lookup; the OS resolves the bare command name.
  return exe;
}
