import * as assert from "assert";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import type { AnvilApi } from "../../src/extension";

export const PROJECT = process.env.ANVIL_TEST_PROJECT ?? "";
export const slash = (text: string) => text.split(path.sep).join("/");

export async function api(): Promise<AnvilApi> {
  const extension = vscode.extensions.getExtension<AnvilApi>("anvil.anvil");
  assert.ok(extension, "the extension is installed");
  const exports = await extension.activate();
  await exports.ready;
  return exports;
}

/** Wait until the check returns something truthy, polling. */
export async function waitFor<T>(what: string, check: () => T | Promise<T>, timeoutMs = 20000): Promise<NonNullable<T>> {
  const started = Date.now();
  for (;;) {
    const value = await check();
    if (value) {
      return value as NonNullable<T>;
    }
    if (Date.now() - started > timeoutMs) {
      throw new Error(`timed out waiting for ${what}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
}

/** The argument lists the fake anvil was called with, oldest first. */
export function fakeCalls(): string[][] {
  const log = path.join(PROJECT, "fake-calls.log");
  if (!fs.existsSync(log)) {
    return [];
  }
  return fs
    .readFileSync(log, "utf8")
    .split(/\r?\n/)
    .filter(Boolean)
    .map((line) => JSON.parse(line) as string[]);
}

export function clearFakeCalls(): void {
  fs.rmSync(path.join(PROJECT, "fake-calls.log"), { force: true });
}

export function hasCall(prefix: string[]): boolean {
  return fakeCalls().some((call) => prefix.every((word, index) => call[index] === word));
}

export function processArgs(task: vscode.Task): string[] {
  const execution = task.execution as vscode.ProcessExecution;
  return execution.args;
}

/** Edit a file through VS Code and save it, as a user would. */
export async function rewrite(file: string, text: string): Promise<void> {
  const document = await vscode.workspace.openTextDocument(file);
  const edit = new vscode.WorkspaceEdit();
  edit.replace(document.uri, new vscode.Range(0, 0, document.lineCount, 0), text);
  await vscode.workspace.applyEdit(edit);
  await document.save();
}
