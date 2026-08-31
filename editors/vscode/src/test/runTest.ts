import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { runTests } from "@vscode/test-electron";

function writeBig(file: string, entries: Record<string, Buffer>) {
  const names = Object.keys(entries);
  let offset = 0x10 + names.reduce((size, name) => size + 8 + Buffer.byteLength(name) + 1, 0);
  const header = Buffer.alloc(offset);
  header.write("BIGF");
  header.writeUInt32BE(offset + names.reduce((size, name) => size + entries[name].length, 0), 4);
  header.writeUInt32BE(names.length, 8);
  let cursor = 0x10;
  for (const name of names) {
    header.writeUInt32BE(offset, cursor);
    header.writeUInt32BE(entries[name].length, cursor + 4);
    cursor += 8;
    cursor += header.write(name, cursor);
    header[cursor++] = 0;
    offset += entries[name].length;
  }
  fs.writeFileSync(file, Buffer.concat([header, ...names.map((name) => entries[name])]));
}

async function main() {
  const extensionDevelopmentPath = path.resolve(__dirname, "../../..");
  const extensionTestsPath = path.resolve(__dirname, "suite/index");
  const testWorkspace = fs.mkdtempSync(path.join(os.tmpdir(), "zerosyntax-vscode-"));
  const testWorkspaceFile = path.join(testWorkspace, "ZeroSyntax.code-workspace");
  const archive = path.join(testWorkspace, "Base Cache #.big");
  writeBig(archive, {
    "Data/INI/Archived.ini": Buffer.from(
      "CommandButton SmokeArchivedButton\n" +
        "  Command = UNIT_BUILD\n" +
        "End\n" +
        "CommandSet SmokeArchivedSet\n" +
        "  1 = SmokeArchivedButton\n" +
        "End\n"
    ),
  });
  fs.writeFileSync(
    testWorkspaceFile,
    JSON.stringify({
      folders: [{ path: "." }],
      settings: {
        "zerosyntax.analysis.allowPercentagesWithoutSign": false,
        "zerosyntax.baseIniRoots": [archive],
      },
    })
  );

  try {
    await runTests({
      extensionDevelopmentPath,
      extensionTestsPath,
      launchArgs: [testWorkspaceFile, "--disable-extensions"],
    });
  } finally {
    fs.rmSync(testWorkspace, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
