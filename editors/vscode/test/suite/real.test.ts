// The extension against a real anvil (ANVIL_REAL) in the `real` fixture: build, compiler errors, tests, and a debug
// session stopped at a breakpoint.

import * as assert from "assert";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import { PROJECT, api, slash, waitFor } from "./helpers";
import { createTask, runTask } from "../../src/tasks";

const ROOT = slash(PROJECT);

suite("real anvil", () => {
  suiteSetup(async () => {
    await api();
  });

  test("reads the model from anvil targets --json", async () => {
    const project = (await api()).project()!;
    assert.ok(project.model, project.error);
    assert.strictEqual(project.model.version, 1);
    assert.strictEqual(project.model.root.toLowerCase(), ROOT.toLowerCase());
    assert.strictEqual(project.model.profile, "debug");
    assert.ok(project.model.profiles.includes("release"));
    const app = project.findTarget("app");
    assert.ok(app?.runnable);
    assert.ok(app.output?.toLowerCase().startsWith(`${ROOT}/out/debug/app`.toLowerCase()), app.output ?? "no output");
    assert.deepStrictEqual(
      project.model.tests.map((test) => test.name).sort(),
      ["app_fails", "app_runs"],
    );
    assert.strictEqual(project.model.tests.find((test) => test.name === "app_runs")?.program, "app");
    console.log(`      toolchain: ${JSON.stringify(project.model.toolchain)}`);
  });

  test("the build task builds the program", async () => {
    const project = (await api()).project()!;
    const code = await runTask(createTask(project, { type: "anvil", command: "build" }));
    assert.strictEqual(code, 0);
    assert.ok(fs.existsSync(project.findTarget("app")!.output!));
  });

  test("a compiler error lands on its file and line in Problems", async () => {
    const project = (await api()).project()!;
    const bad = path.join(PROJECT, "src", "bad.cpp");
    fs.writeFileSync(bad, "int f() {\n    return undeclared_name;\n}\n");
    try {
      const code = await runTask(createTask(project, { type: "anvil", command: "build" }));
      assert.notStrictEqual(code, 0);
      const diagnostics = await waitFor("the error in Problems", () => {
        const found = vscode.languages.getDiagnostics(vscode.Uri.file(bad));
        return found.length > 0 ? found : undefined;
      });
      assert.strictEqual(diagnostics[0].range.start.line, 1);
      assert.match(diagnostics[0].message, /undeclared_name/);
    } finally {
      fs.rmSync(bad, { force: true });
    }
  });

  test("Test Explorer runs the tests with anvil test", async () => {
    const tests = (await api()).tests;
    await tests.run(new vscode.TestRunRequest(), new vscode.CancellationTokenSource().token);
    assert.strictEqual(tests.outcomes.get("app_runs")?.outcome, "passed", tests.outcomes.get("app_runs")?.message);
    const failed = tests.outcomes.get("app_fails");
    assert.strictEqual(failed?.outcome, "failed");
    assert.match(failed!.message, /failing on purpose/);
  });

  test("Even Better TOML flags an unknown key with anvil's own schema", async function () {
    if (!vscode.extensions.getExtension("tamasfe.even-better-toml")) {
      this.skip();
    }
    const file = path.join(PROJECT, "anvil.toml");
    const old = fs.readFileSync(file, "utf8");
    const document = await vscode.workspace.openTextDocument(file);
    await vscode.window.showTextDocument(document);
    const edit = new vscode.WorkspaceEdit();
    edit.insert(document.uri, new vscode.Position(6, 0), 'linkflags = ["/DEBUG"]\n');
    await vscode.workspace.applyEdit(edit);
    try {
      const diagnostics = await waitFor(
        "a schema diagnostic",
        () => {
          const found = vscode.languages.getDiagnostics(document.uri).filter((diagnostic) => diagnostic.source !== "anvil");
          return found.length > 0 ? found : undefined;
        },
        40000,
      );
      console.log(`      ${diagnostics.map((diagnostic) => `${diagnostic.source}: ${diagnostic.message}`).join(" | ")}`);
      assert.ok(diagnostics.some((diagnostic) => diagnostic.message.includes("linkflags")));
    } finally {
      await vscode.commands.executeCommand("workbench.action.files.revert");
      assert.strictEqual(fs.readFileSync(file, "utf8"), old);
    }
  });

  test("F5: builds, starts the C/C++ debugger and stops at a breakpoint", async function () {
    if (!vscode.extensions.getExtension("ms-vscode.cpptools")) {
      this.skip();
    }
    this.timeout(180000);
    // What F5 runs when there is no launch.json
    await debugToBreakpoint(() => vscode.commands.executeCommand("anvil.debugTarget").then(() => true));
  });

  test("F5 with gcc: cppdbg and gdb stop at the breakpoint", async function () {
    if (!vscode.extensions.getExtension("ms-vscode.cpptools") || !process.env.ANVIL_TEST_GCC) {
      this.skip();
    }
    this.timeout(180000);
    const project = (await api()).project()!;
    await project.setToolchain("gcc");
    try {
      assert.strictEqual(project.model?.toolchain.debugger, "gdb");
      await debugToBreakpoint(() =>
        vscode.debug.startDebugging(project.folder, { type: "anvil", request: "launch", name: "Anvil: app", target: "app", console: "internalConsole" }),
      );
    } finally {
      await project.setToolchain(undefined);
    }
  });
});

/** Start the anvil launch configuration for app with a breakpoint in main, check where it stopped and a local's value. */
async function debugToBreakpoint(start: () => Thenable<boolean>): Promise<void> {
  const project = (await api()).project()!;
  const source = vscode.Uri.file(path.join(PROJECT, "src", "main.cpp"));
  const line = fs
    .readFileSync(source.fsPath, "utf8")
    .split(/\r?\n/)
    .findIndex((text) => text.includes("breakpoint"));
  const breakpoint = new vscode.SourceBreakpoint(new vscode.Location(source, new vscode.Position(line, 0)));
  vscode.debug.addBreakpoints([breakpoint]);

  // Watch what the debug adapter says
  let stopped: { threadId: number; reason: string } | undefined;
  let output = "";
  const tracker = vscode.debug.registerDebugAdapterTrackerFactory("*", {
    createDebugAdapterTracker: () => ({
      onDidSendMessage: (message: { type: string; event?: string; body?: { threadId: number; reason: string; output?: string } }) => {
        if (message.type === "event" && message.event === "stopped" && message.body) {
          stopped = message.body;
        }
        if (message.type === "event" && message.event === "output" && message.body?.output) {
          output += message.body.output;
        }
      },
    }),
  });

  try {
    assert.ok(await start(), "the debugger started");
    const session = await waitFor("a debug session", () => vscode.debug.activeDebugSession);
    assert.strictEqual(session.type, project.model!.toolchain.debugger === "msvc" ? "cppvsdbg" : "cppdbg");

    const stop = await waitFor("the breakpoint", () => stopped, 120000);
    assert.strictEqual(stop.reason, "breakpoint");
    const trace = await session.customRequest("stackTrace", { threadId: stop.threadId, startFrame: 0, levels: 1 });
    const frame = trace.stackFrames[0];
    assert.strictEqual(frame.line, line + 1);
    assert.match(slash(frame.source.path), /src\/main\.cpp$/i);
    const answer = await session.customRequest("evaluate", { expression: "answer", frameId: frame.id, context: "watch" });
    assert.strictEqual(answer.result, "42");
    console.log(`      ${session.type}: stopped at ${frame.source.path}:${frame.line} in ${frame.name}; answer = ${answer.result}`);

    await session.customRequest("continue", { threadId: stop.threadId });
    await waitFor("the program to end", () => vscode.debug.activeDebugSession === undefined, 60000);
    console.log(`      debug console: ${JSON.stringify(output.slice(-200))}`);
  } finally {
    tracker.dispose();
    vscode.debug.removeBreakpoints([breakpoint]);
    if (vscode.debug.activeDebugSession) {
      await vscode.debug.stopDebugging();
    }
  }
}
