// Test Explorer: the project's tests, run with `anvil test` (results read from its JUnit report) or debugged.

import * as childProcess from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import { Test, killTree, quoteCommand, shellArgs, spawnOptions } from "./anvil";
import { DEBUG_TYPE } from "./debug";
import { Project } from "./project";

const PACKAGE_PREFIX = "package:";

interface CaseResult {
  name: string;
  seconds: number;
  outcome: "passed" | "failed" | "skipped" | "errored";
  message: string;
  output: string;
}

export class AnvilTests implements vscode.Disposable {
  readonly controller = vscode.tests.createTestController("anvil", "Anvil");
  /** Each test's last result in this session: passed, failed, skipped or errored, with its message. */
  readonly outcomes = new Map<string, { outcome: string; message: string }>();

  constructor(private readonly project: () => Project | undefined) {
    this.controller.createRunProfile("Run", vscode.TestRunProfileKind.Run, (request, token) => this.run(request, token), true);
    this.controller.createRunProfile("Debug", vscode.TestRunProfileKind.Debug, (request, token) => this.debug(request, token), true);
    this.controller.refreshHandler = async () => {
      await this.project()?.refresh();
    };
  }

  dispose(): void {
    this.controller.dispose();
  }

  /** Show the model's tests: grouped by package when a name has one (`pkg/name`). */
  async update(): Promise<void> {
    const project = this.project();
    const tests = project?.model?.tests ?? [];
    const locations = project ? await testLocations(project, tests) : new Map<string, vscode.Location>();

    const top: vscode.TestItem[] = [];
    const packages = new Map<string, vscode.TestItem>();
    for (const test of tests) {
      const slash = test.name.lastIndexOf("/");
      const location = locations.get(test.name);
      const item = this.controller.createTestItem(test.name, slash >= 0 ? test.name.slice(slash + 1) : test.name, location?.uri);
      item.range = location?.range;
      item.description = test.program ?? test.command[0];

      if (slash < 0) {
        top.push(item);
        continue;
      }
      const packageName = test.name.slice(0, slash);
      let group = packages.get(packageName);
      if (!group) {
        group = this.controller.createTestItem(PACKAGE_PREFIX + packageName, packageName);
        packages.set(packageName, group);
        top.push(group);
      }
      group.children.add(item);
    }
    this.controller.items.replace(top);
  }

  async run(request: vscode.TestRunRequest, token: vscode.CancellationToken): Promise<void> {
    const project = this.project();
    const run = this.controller.createTestRun(request);
    try {
      if (!project?.model) {
        return;
      }
      const all = project.model.tests.map((test) => test.name);
      const wanted = new Set(this.selectedNames(request));
      for (const name of wanted) {
        const item = this.findItem(name);
        if (item) {
          run.enqueued(item);
        }
      }

      // Each `anvil test <filter>` runs the tests whose name contains the filter; pick filters that run nothing unwanted
      const remaining = new Set(wanted);
      while (remaining.size > 0 && !token.isCancellationRequested) {
        const filter = chooseFilter([...remaining], all, wanted);
        const runs = all.filter((name) => filter === undefined || name.includes(filter));
        for (const name of runs) {
          const item = this.findItem(name);
          if (item) {
            run.started(item);
          }
        }
        const results = await this.runAnvilTest(project, filter, run, token);
        for (const name of runs) {
          remaining.delete(name);
          const item = this.findItem(name);
          if (!item) {
            continue;
          }
          const result = results.cases.get(name);
          if (!result) {
            const message = token.isCancellationRequested ? "cancelled" : `did not run: ${results.failure ?? "anvil test did not report it"}`;
            run.errored(item, new vscode.TestMessage(message));
            this.outcomes.set(name, { outcome: "errored", message });
          } else {
            report(run, item, result);
            this.outcomes.set(name, { outcome: result.outcome, message: [result.message, result.output].filter(Boolean).join("\n") });
          }
        }
      }
    } finally {
      run.end();
    }
  }

  /** Debug one test's program with the test's arguments and folder (after building it). */
  private async debug(request: vscode.TestRunRequest, token: vscode.CancellationToken): Promise<void> {
    const project = this.project();
    const names = this.selectedNames(request);
    if (!project || names.length === 0) {
      return;
    }
    const run = this.controller.createTestRun(request, undefined, false);
    try {
      const name = names[0];
      if (names.length > 1) {
        run.appendOutput(`Debugging ${name}; the debugger runs one test at a time.\r\n`);
      }
      const item = this.findItem(name);
      if (item) {
        run.started(item);
      }
      const started = await vscode.debug.startDebugging(project.folder, {
        type: DEBUG_TYPE,
        request: "launch",
        name: `Anvil test: ${name}`,
        test: name,
      }, { testRun: run });
      if (started && !token.isCancellationRequested) {
        await new Promise<void>((resolve) => {
          const ended = vscode.debug.onDidTerminateDebugSession(() => {
            ended.dispose();
            resolve();
          });
          token.onCancellationRequested(() => {
            ended.dispose();
            vscode.debug.stopDebugging();
            resolve();
          });
        });
      }
      if (item) {
        if (started) {
          run.skipped(item);
        } else {
          run.errored(item, new vscode.TestMessage("the debugger did not start"));
        }
      }
    } finally {
      run.end();
    }
  }

  private selectedNames(request: vscode.TestRunRequest): string[] {
    const tests = this.project()?.model?.tests ?? [];
    const excluded = new Set(request.exclude?.map((item) => item.id) ?? []);
    const included = request.include ? new Set(request.include.map((item) => item.id)) : undefined;
    return tests
      .map((test) => test.name)
      .filter((name) => {
        const group = PACKAGE_PREFIX + name.slice(0, Math.max(0, name.lastIndexOf("/")));
        if (excluded.has(name) || excluded.has(group)) {
          return false;
        }
        return !included || included.has(name) || included.has(group);
      });
  }

  findItem(name: string): vscode.TestItem | undefined {
    const top = this.controller.items.get(name);
    if (top) {
      return top;
    }
    const slash = name.lastIndexOf("/");
    return slash >= 0 ? this.controller.items.get(PACKAGE_PREFIX + name.slice(0, slash))?.children.get(name) : undefined;
  }

  private runAnvilTest(
    project: Project,
    filter: string | undefined,
    run: vscode.TestRun,
    token: vscode.CancellationToken,
  ): Promise<{ cases: Map<string, CaseResult>; failure?: string }> {
    const anvil = project.requireAnvil();
    const junit = path.join(os.tmpdir(), `anvil-vscode-${process.pid}-${Date.now()}.xml`);
    const args = ["test", ...(filter !== undefined ? [filter] : []), ...project.planArgs(), "--show-output", "--junit", junit];
    run.appendOutput(`> anvil ${args.join(" ")}\r\n`);

    return new Promise((resolve) => {
      const child = childProcess.spawn(quoteCommand(anvil), shellArgs(anvil, args), spawnOptions(anvil, project.manifestDir));
      let tail = "";
      const append = (chunk: string) => {
        tail = (tail + chunk).slice(-4000);
        run.appendOutput(chunk.replace(/\r?\n/g, "\r\n"));
      };
      child.stdout.setEncoding("utf8").on("data", append);
      child.stderr.setEncoding("utf8").on("data", append);
      const cancel = token.onCancellationRequested(() => killTree(child));
      const finish = (code: number | null, error?: Error) => {
        cancel.dispose();
        let cases = new Map<string, CaseResult>();
        try {
          cases = parseJunit(fs.readFileSync(junit, "utf8"));
          fs.unlinkSync(junit);
        } catch {
          // no report: the build failed, or anvil did not start
        }
        const failure = error ? error.message : `anvil test exited with code ${code}\n${tail.trim().split(/\r?\n/).slice(-15).join("\n")}`;
        resolve({ cases, failure });
      };
      child.on("error", (error) => finish(null, error));
      child.on("close", (code) => finish(code));
    });
  }
}

/** The filter for the next `anvil test`: the whole run when every test is wanted, else a package or a test name that matches only wanted tests. */
export function chooseFilter(remaining: string[], all: string[], wanted: Set<string>): string | undefined {
  if (all.every((name) => wanted.has(name))) {
    return undefined;
  }
  const onlyWanted = (filter: string) => all.every((name) => !name.includes(filter) || wanted.has(name));
  const packages = new Set(remaining.filter((name) => name.includes("/")).map((name) => name.slice(0, name.lastIndexOf("/") + 1)));
  for (const prefix of packages) {
    if (onlyWanted(prefix)) {
      return prefix;
    }
  }
  return remaining.find(onlyWanted) ?? remaining[0];
}

/** Read anvil's JUnit report: one testcase per test that ran. */
export function parseJunit(xml: string): Map<string, CaseResult> {
  const cases = new Map<string, CaseResult>();
  const testcase = /<testcase\s([^>]*?)(\/>|>([\s\S]*?)<\/testcase>)/g;
  for (const match of xml.matchAll(testcase)) {
    const attributes = parseAttributes(match[1]);
    const body = match[3] ?? "";
    const name = attributes.get("name") ?? "";
    const result: CaseResult = { name, seconds: Number(attributes.get("time") ?? 0), outcome: "passed", message: "", output: "" };

    const failure = /<(failure|error)\b([^>]*?)(?:\/>|>([\s\S]*?)<\/\1>)/.exec(body);
    if (failure) {
      result.outcome = failure[1] === "failure" ? "failed" : "errored";
      result.message = parseAttributes(failure[2]).get("message") ?? "";
      result.output = unescapeXml(failure[3] ?? "");
    } else if (/<skipped\b/.test(body)) {
      result.outcome = "skipped";
    }
    cases.set(name, result);
  }
  return cases;
}

function report(run: vscode.TestRun, item: vscode.TestItem, result: CaseResult): void {
  const milliseconds = result.seconds * 1000;
  switch (result.outcome) {
    case "passed":
      run.passed(item, milliseconds);
      break;
    case "skipped":
      run.skipped(item);
      break;
    case "failed": {
      const text = [result.message, result.output.trim()].filter(Boolean).join("\n\n");
      const message = new vscode.TestMessage(text || "failed");
      if (item.uri && item.range) {
        message.location = new vscode.Location(item.uri, item.range);
      }
      run.failed(item, message, milliseconds);
      break;
    }
    case "errored":
      run.errored(item, new vscode.TestMessage(result.message || "interrupted"), milliseconds);
      break;
  }
}

function parseAttributes(text: string): Map<string, string> {
  const attributes = new Map<string, string>();
  for (const match of text.matchAll(/([\w:-]+)="([^"]*)"/g)) {
    attributes.set(match[1], unescapeXml(match[2]));
  }
  return attributes;
}

function unescapeXml(text: string): string {
  return text
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&apos;/g, "'")
    .replace(/&#(\d+);/g, (_, code: string) => String.fromCodePoint(Number(code)))
    .replace(/&amp;/g, "&");
}

/**
 * Where each test is declared, as the language server's document symbols of anvil.toml and tests.toml files say: a
 * [[test]] table's name (a matrix's `name = "a_{v}"` covers each test it makes), or a kind = "test" target. None
 * while the server is not running.
 */
async function testLocations(project: Project, tests: Test[]): Promise<Map<string, vscode.Location>> {
  const locations = new Map<string, vscode.Location>();
  if (tests.length === 0) {
    return locations;
  }
  const files = await vscode.workspace.findFiles(
    new vscode.RelativePattern(vscode.Uri.file(project.root), "**/{anvil,tests}.toml"),
    "**/{out,node_modules,.git,vcpkg_installed}/**",
  );

  // Each declaration: the names it matches and where its name is written
  const declarations: { name: string; matches: (name: string) => boolean; location: vscode.Location }[] = [];
  for (const file of files.sort((left, right) => left.fsPath.localeCompare(right.fsPath))) {
    const symbols = await vscode.commands
      .executeCommand<vscode.DocumentSymbol[] | undefined>("vscode.executeDocumentSymbolProvider", file)
      .then((found) => found ?? [], () => []);
    for (const symbol of symbols) {
      const declared = symbol.detail === "[[test]]" ? [symbol] : symbol.name === "[target]" ? (symbol.children ?? []).filter((child) => child.detail === "test") : [];
      for (const test of declared) {
        declarations.push({ name: test.name, matches: namePattern(test.name), location: new vscode.Location(file, test.selectionRange ?? test.range) });
      }
    }
  }

  // A name written out wins over a matrix's pattern
  for (const test of tests) {
    const short = test.name.slice(test.name.lastIndexOf("/") + 1);
    const found = declarations.find((declaration) => declaration.name === short) ?? declarations.find((declaration) => declaration.matches(short));
    if (found) {
      locations.set(test.name, found.location);
    }
  }
  return locations;
}

/** Whether a test's name is one a declared name makes: itself, or a matrix name with each `{param}` filled in. */
function namePattern(declared: string): (name: string) => boolean {
  if (!declared.includes("{")) {
    return (name) => name === declared;
  }
  const parts = declared.split(/\{[^}]*\}/).map((part) => part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  const pattern = new RegExp(`^${parts.join(".+")}$`);
  return (name) => pattern.test(name);
}
