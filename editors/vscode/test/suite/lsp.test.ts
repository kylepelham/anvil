// The language server of a real anvil (`anvil lsp`) through the extension, in the `lsp` fixture project: the
// manifest's diagnostics come from it alone, its snippets, and the status bar's profile reaching it.

import * as assert from "assert";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import { PROJECT, api, rewrite, waitFor } from "./helpers";

const MANIFEST = path.join(PROJECT, "anvil.toml");
const TESTS_FILE = path.join(PROJECT, "tests", "tests.toml");

/** Every diagnostic of a file, from any source. */
const diagnosticsOf = (file: string) => vscode.languages.getDiagnostics(vscode.Uri.file(file));

async function completions(file: string, position: vscode.Position): Promise<vscode.CompletionItem[]> {
  const list = await vscode.commands.executeCommand<vscode.CompletionList>("vscode.executeCompletionItemProvider", vscode.Uri.file(file), position);
  return list.items;
}

const labelOf = (item: vscode.CompletionItem) => (typeof item.label === "string" ? item.label : item.label.label);

async function hoverText(file: string, position: vscode.Position): Promise<string> {
  const hovers = await vscode.commands.executeCommand<vscode.Hover[]>("vscode.executeHoverProvider", vscode.Uri.file(file), position);
  return hovers
    .flatMap((hover) => hover.contents)
    .map((content) => (typeof content === "string" ? content : content.value))
    .join("\n");
}

suite("anvil lsp", () => {
  suiteSetup(async () => {
    const anvil = await api();
    await waitFor("anvil lsp running", () => anvil.languageServer.running || anvil.languageServer.failure, 30000);
    assert.strictEqual(anvil.languageServer.failure, undefined);
    const document = await vscode.workspace.openTextDocument(MANIFEST);
    await vscode.window.showTextDocument(document);
  });

  test("anvil.toml and tests.toml are Anvil TOML, with the extension's grammar", async () => {
    const manifest = await vscode.workspace.openTextDocument(MANIFEST);
    assert.strictEqual(manifest.languageId, "anvil-toml");
    assert.strictEqual((await vscode.workspace.openTextDocument(TESTS_FILE)).languageId, "anvil-toml");

    // The grammar's scopes (an internal command of VS Code's own colorizer tests; skipped when it is gone)
    type Token = { c: string; t: string };
    const tokens = await Promise.resolve(vscode.commands.executeCommand<Token[]>("_workbench.captureSyntaxTokens", manifest.uri)).catch(() => undefined);
    if (tokens) {
      const scopes = (text: string) => tokens.find((token) => token.c.trim() === text)?.t ?? JSON.stringify(tokens);
      assert.match(scopes("target"), /support\.type\.property-name\.table\.toml/);
      assert.match(scopes("kind"), /support\.type\.property-name\.toml/);
      assert.match(scopes("exe"), /string\.quoted\.double/);
    } else {
      console.log("      (no _workbench.captureSyntaxTokens: the grammar's scopes are not checked)");
    }
  });

  test("a manifest error: exactly one diagnostic, from anvil, on its word; fixing it clears it", async () => {
    const old = fs.readFileSync(MANIFEST, "utf8");
    assert.deepStrictEqual(diagnosticsOf(MANIFEST), [], "the fixture is clean");

    // Saved, so `anvil targets --json` fails too: that shows in the status bar, not in Problems
    await rewrite(MANIFEST, old.replace('version = "0.1.0"', 'version = "0.1.0"\nbogus = 1'));
    try {
      const anvil = await api();
      await waitFor("the server's diagnostic", () => diagnosticsOf(MANIFEST).length > 0);
      await waitFor("anvil targets --json to fail", () => anvil.project()?.error);
      await new Promise((resolve) => setTimeout(resolve, 1500));

      const diagnostics = diagnosticsOf(MANIFEST);
      assert.strictEqual(diagnostics.length, 1, diagnostics.map((diagnostic) => `${diagnostic.source}: ${diagnostic.message}`).join("\n"));
      const [diagnostic] = diagnostics;
      assert.strictEqual(diagnostic.source, "anvil");
      assert.strictEqual(diagnostic.severity, vscode.DiagnosticSeverity.Error);
      assert.match(diagnostic.message, /bogus/);
      assert.deepStrictEqual(
        [diagnostic.range.start.line, diagnostic.range.start.character, diagnostic.range.end.line, diagnostic.range.end.character],
        [3, 0, 3, 5],
      );
    } finally {
      await rewrite(MANIFEST, old);
    }
    await waitFor("the diagnostic to clear", () => diagnosticsOf(MANIFEST).length === 0);
    await waitFor("the model to be read again", async () => (await api()).project()?.error === undefined);
  });

  test("an unsaved edit is checked as typed", async () => {
    const document = await vscode.workspace.openTextDocument(MANIFEST);
    const edit = new vscode.WorkspaceEdit();
    edit.replace(document.uri, document.lineAt(5).range, 'kind = "exee"');
    await vscode.workspace.applyEdit(edit);
    try {
      const diagnostics = await waitFor("the diagnostic of the edit", () => {
        const found = diagnosticsOf(MANIFEST);
        return found.length > 0 ? found : undefined;
      });
      assert.strictEqual(diagnostics.length, 1);
      assert.strictEqual(diagnostics[0].range.start.line, 5);
    } finally {
      await vscode.commands.executeCommand("workbench.action.files.revert");
    }
    await waitFor("the diagnostic to clear", () => diagnosticsOf(MANIFEST).length === 0);
  });

  test("completion offers table snippets; tests.toml only [[test]] ones", async () => {
    const manifest = await vscode.workspace.openTextDocument(MANIFEST);
    const items = await completions(MANIFEST, new vscode.Position(manifest.lineCount - 1, 0));
    const snippet = items.find((item) => labelOf(item) === "[target.name]");
    assert.ok(snippet, items.map(labelOf).join(", "));
    assert.strictEqual(snippet.kind, vscode.CompletionItemKind.Snippet);
    assert.ok(snippet.insertText instanceof vscode.SnippetString, "inserted as a snippet");
    assert.match(snippet.insertText.value, /^\[target\.\$\{1:name\}\]/);
    for (const wanted of ["[deps]", "[[test]]", "[profile.name]"]) {
      assert.ok(items.some((item) => labelOf(item) === wanted), wanted);
    }

    // The keys [target.app] takes come from the server too
    assert.ok(items.some((item) => labelOf(item) === "sources" && item.kind === vscode.CompletionItemKind.Property));

    const testsDocument = await vscode.workspace.openTextDocument(TESTS_FILE);
    const testsItems = await completions(TESTS_FILE, new vscode.Position(testsDocument.lineCount - 1, 0));
    const snippets = testsItems.filter((item) => item.kind === vscode.CompletionItemKind.Snippet).map(labelOf);
    assert.deepStrictEqual(snippets.sort(), ["[[test]]", "[[test]] with expect"]);
  });

  test("the status bar's profile reaches the server", async () => {
    const anvil = await api();
    const project = anvil.project()!;
    const debugWord = new vscode.Position(6, 6);
    await waitFor("hover with the checked build", async () => /in the checked build \(debug,/.test(await hoverText(MANIFEST, debugWord)), 60000);

    await project.setProfile("release");
    try {
      assert.strictEqual(anvil.languageServer.sentSettings?.profile, "release");
      const text = await waitFor(
        "the server to check the release build",
        async () => {
          const hover = await hoverText(MANIFEST, debugWord);
          return /in the checked build \(release,/.test(hover) ? hover : undefined;
        },
        60000,
      );
      assert.match(text, /`false` in the checked build/);
    } finally {
      await project.setProfile(undefined);
    }
    assert.strictEqual(anvil.languageServer.sentSettings?.profile, "debug");
  });
});
