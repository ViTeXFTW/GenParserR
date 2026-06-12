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
    // Evaluated on every (re)start, so a settings-triggered restart picks up
    // the current values. The server reads these once at `initialize`.
    initializationOptions: () => ({
      format: {
        enable: vscode.workspace
          .getConfiguration("genparser")
          .get<boolean>("format.enable", false),
      },
    }),
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

  // Server settings (initializationOptions, server path) are read once at
  // startup, so any genparser.* change needs a clean restart to apply.
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("genparser")) {
        void client?.restart();
      }
    })
  );
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
    return bundled;
  }

  // Fall back to PATH lookup; the OS resolves the bare command name.
  return exe;
}
