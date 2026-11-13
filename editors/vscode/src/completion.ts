// Snippets for the common anvil.toml and tests.toml tables, offered in those files only.

import * as vscode from "vscode";

interface TableSnippet {
  label: string;
  detail: string;
  body: string;
  /** Offered in tests.toml too, which holds only [[test]] tables. */
  tests?: boolean;
}

const SNIPPETS: TableSnippet[] = [
  {
    label: "[target.name]",
    detail: "A target: program, library, test...",
    body: '[target.${1:name}]\nkind = "${2|exe,lib,shared,object,headers,test,custom|}"\nsources = ["${3:src/**/*.cpp}"]\ndeps = [$4]\n$0',
  },
  {
    label: "[target.name] (program)",
    detail: "A program with its own folder",
    body: '[target.${1:app}]\nkind = "exe"\ndir = "${2:$1}"\ndeps = [$3]\n$0',
  },
  {
    label: "[target.name] (library)",
    detail: "A static library with public headers in include/",
    body: '[target.${1:core}]\nkind = "lib"\ndir = "${2:$1}"\n\n[target.$1.public]\ninclude = ["include"]\ndeps = [$3]\n$0',
  },
  {
    label: "[deps]",
    detail: "Dependencies: vcpkg ports, git, path, pkg-config, system",
    body: '[deps]\n${1:fmt} = "${2|vcpkg,system,pkg-config|}"\n$0',
  },
  {
    label: "[deps] git",
    detail: "A git dependency",
    body: '${1:name} = { git = "${2:https://github.com/owner/repo}", tag = "${3:v1.0.0}" }\n$0',
  },
  {
    label: "[dev-deps]",
    detail: "Dependencies of the tests only",
    body: '[dev-deps]\n${1:gtest} = "vcpkg"\n$0',
  },
  {
    label: "[[test]]",
    detail: "A test that runs a command",
    body: '[[test]]\nname = "${1:name}"\ncommand = ["{target:${2:app}}"${3:, "--arg"}]\n$0',
    tests: true,
  },
  {
    label: "[[test]] with expect",
    detail: "A test judged by its output",
    body: '[[test]]\nname = "${1:name}"\ncommand = ["{target:${2:app}}"]\nexpect = ["${3:regex}"]\nexit-code = ${4:0}\n$0',
    tests: true,
  },
  {
    label: "[profile.name]",
    detail: "A build profile",
    body: '[profile.${1:shipping}]\ninherits = "${2|release,debug|}"\n${3:lto = true}\n$0',
  },
  {
    label: "[options]",
    detail: "Build options (-o name=true|false)",
    body: '[options]\n${1:feature} = { default = ${2|true,false|}, description = "${3:What it turns on}" }\n$0',
  },
  {
    label: "[[target.name.run]]",
    detail: "A build step that writes files",
    body: '[[target.${1:app}.run]]\nfor-each = "${2:shaders/*.hlsl}"\noutput = "{gen}/${3:{stem}.h}"\ncommand = "${4:tool {input} -o {output}}"\n$0',
  },
];

export class SnippetCompletions implements vscode.CompletionItemProvider {
  provideCompletionItems(document: vscode.TextDocument, position: vscode.Position): vscode.CompletionItem[] {
    // Only where a table header can start: the line so far is empty or a partial header
    const before = document.lineAt(position.line).text.slice(0, position.character);
    if (!/^\s*\[*[\w.\- ]*$/.test(before)) {
      return [];
    }
    const testsFile = /(^|[\\/])tests\.toml$/i.test(document.fileName);
    const start = before.length - before.trimStart().length;
    const range = new vscode.Range(position.line, start, position.line, position.character);

    return SNIPPETS.filter((snippet) => !testsFile || snippet.tests).map((snippet) => {
      const item = new vscode.CompletionItem(snippet.label, vscode.CompletionItemKind.Snippet);
      item.insertText = new vscode.SnippetString(snippet.body);
      item.detail = snippet.detail;
      item.range = range;
      item.filterText = snippet.label;
      item.sortText = "~" + snippet.label;
      return item;
    });
  }
}
