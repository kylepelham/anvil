// anvil.toml and tests.toml schemas for Even Better TOML: written by `anvil schema`, kept in global storage.

import * as crypto from "crypto";
import * as fs from "fs";
import * as path from "path";
import * as url from "url";
import * as vscode from "vscode";
import { runAnvil } from "./anvil";

const SCHEMA_KEY = "anvil.schemaSource";

interface SchemaFile {
  /** Matches the `fileMatch` of a `tomlValidation` entry in package.json. */
  fileMatch: string;
  file: string;
  args: string[];
}

function schemaFiles(context: vscode.ExtensionContext): SchemaFile[] {
  const directory = context.globalStorageUri.fsPath;
  return [
    { fileMatch: "anvil.toml", file: path.join(directory, "anvil.schema.json"), args: ["schema"] },
    { fileMatch: "tests.toml", file: path.join(directory, "tests.schema.json"), args: ["schema", "--tests"] },
  ];
}

/**
 * Point the extension's `tomlValidation` entries at the schema files. package.json can only hold a fixed URL, so this
 * changes the in-memory manifest that Even Better TOML reads when it starts; it must run before that, at activation.
 */
export function pointSchemaAssociations(context: vscode.ExtensionContext): void {
  const rules = context.extension.packageJSON?.contributes?.tomlValidation;
  if (!Array.isArray(rules)) {
    return;
  }
  for (const schema of schemaFiles(context)) {
    const rule = rules.find((candidate: { fileMatch?: string }) => candidate.fileMatch === schema.fileMatch);
    if (rule) {
      rule.url = url.pathToFileURL(schema.file).href;
    }
  }
}

/** Write the schemas when they are missing or anvil changed since they were written. */
export async function updateSchemas(context: vscode.ExtensionContext, anvil: string, output: vscode.OutputChannel): Promise<boolean> {
  const files = schemaFiles(context);
  const source = anvilIdentity(anvil);
  if (!source) {
    return false;
  }
  const current = context.globalState.get<string>(SCHEMA_KEY);
  if (current === source && files.every((schema) => fs.existsSync(schema.file))) {
    return false;
  }

  await fs.promises.mkdir(context.globalStorageUri.fsPath, { recursive: true });
  let changed = false;
  for (const schema of files) {
    const result = await runAnvil(anvil, schema.args, context.globalStorageUri.fsPath);
    if (result.code !== 0) {
      output.appendLine(`anvil ${schema.args.join(" ")} failed: ${result.stderr.trim()}`);
      return changed;
    }
    // Rewrite only on change, so the TOML extension reloads nothing for nothing
    let old: string | undefined;
    try {
      old = await fs.promises.readFile(schema.file, "utf8");
    } catch {
      old = undefined;
    }
    if (old !== result.stdout) {
      await fs.promises.writeFile(schema.file, result.stdout, "utf8");
      changed = true;
    }
  }
  await context.globalState.update(SCHEMA_KEY, source);
  return changed;
}

/** Which anvil binary this is: its path, size and modification time (a rebuilt anvil may keep its version). */
function anvilIdentity(anvil: string): string | undefined {
  try {
    const stat = fs.statSync(anvil);
    return crypto.createHash("sha1").update(`${path.resolve(anvil)}|${stat.size}|${stat.mtimeMs}`).digest("hex");
  } catch {
    return undefined;
  }
}
