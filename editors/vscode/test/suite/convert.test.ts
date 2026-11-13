// A CMake project with no anvil.toml: the extension waits until Anvil: Convert CMake Project writes one.

import * as assert from "assert";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import { PROJECT, api, hasCall, waitFor } from "./helpers";

suite("convert", () => {
  test("no project before the conversion", async () => {
    assert.strictEqual(fs.existsSync(path.join(PROJECT, "anvil.toml")), false);
    const anvil = await api();
    assert.strictEqual(anvil.project(), undefined);
    assert.strictEqual((await vscode.tasks.fetchTasks({ type: "anvil" })).length, 0);
  });

  test("Anvil: Convert CMake Project runs anvil convert, then the project is there", async () => {
    await vscode.commands.executeCommand("anvil.convert");
    assert.ok(hasCall(["convert"]));
    const anvil = await api();
    const project = await waitFor("the project", () => anvil.project()?.model && anvil.project());
    assert.strictEqual(project.selectedTarget?.qualified, "game");
    const editor = vscode.window.activeTextEditor;
    assert.ok(editor?.document.uri.fsPath.endsWith("anvil.toml"), "anvil.toml is opened");
  });
});
