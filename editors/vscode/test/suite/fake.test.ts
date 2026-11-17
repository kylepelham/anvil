// The extension against the fake anvil (test/fixtures/fake-anvil) in the `hello` fixture project.

import * as assert from "assert";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import { PROJECT, api, clearFakeCalls, fakeCalls, hasCall, processArgs, rewrite, slash, waitFor } from "./helpers";
import { chooseFilter, parseJunit } from "../../src/testing";

const ROOT = slash(vscode.workspace.workspaceFolders![0].uri.fsPath);
const exe = process.platform === "win32" ? ".exe" : "";

suite("fake anvil", () => {
  suiteSetup(async () => {
    await api();
  });

  test("activates on anvil.toml and reads the model", async () => {
    const anvil = await api();
    const project = anvil.project();
    assert.ok(project, "a project");
    assert.ok(project.model, project.error);
    assert.strictEqual(project.model.root, ROOT);
    assert.strictEqual(project.profile, "debug");
    assert.strictEqual(project.selectedTarget?.qualified, "game");
    assert.deepStrictEqual(
      project.runnableTargets.map((target) => target.qualified),
      ["game", "tool", "dep/plugin"],
    );
    assert.ok(hasCall(["targets", "--json", "-p", "debug"]));
  });

  test("provides build, run, test and clean tasks", async () => {
    const tasks = await vscode.tasks.fetchTasks({ type: "anvil" });
    const byName = new Map(tasks.map((task) => [task.name, task]));
    for (const name of ["build", "build game", "build core", "run game", "run tool", "run dep/plugin", "test", "clean"]) {
      assert.ok(byName.has(name), `task ${name} in ${[...byName.keys()].join(", ")}`);
    }

    const build = byName.get("build")!;
    assert.strictEqual(build.source, "anvil");
    assert.deepStrictEqual(processArgs(build), ["build", "-p", "debug"]);
    assert.strictEqual(build.group?.id, vscode.TaskGroup.Build.id);
    assert.strictEqual(build.group?.isDefault, true);
    assert.deepStrictEqual(build.problemMatchers, ["$anvil-msvc"]);
    assert.strictEqual(slash((build.execution as vscode.ProcessExecution).options!.cwd!), ROOT);

    assert.deepStrictEqual(processArgs(byName.get("run game")!), ["run", "game", "-p", "debug"]);
    assert.deepStrictEqual(processArgs(byName.get("build core")!), ["build", "core", "-p", "debug"]);
    assert.deepStrictEqual(processArgs(byName.get("clean")!), ["clean", "-p", "debug"]);
    assert.strictEqual(byName.get("test")!.group?.id, vscode.TaskGroup.Test.id);
  });

  test("the default build task runs anvil build without asking", async () => {
    clearFakeCalls();
    await vscode.commands.executeCommand("workbench.action.tasks.build");
    await waitFor("anvil build", () => hasCall(["build", "-p", "debug"]));
  });

  test("commands run anvil with the selections", async () => {
    clearFakeCalls();
    await vscode.commands.executeCommand("anvil.run");
    await waitFor("anvil run", () => hasCall(["run", "game", "-p", "debug"]));
    await vscode.commands.executeCommand("anvil.buildTarget");
    await waitFor("anvil build game", () => hasCall(["build", "game", "-p", "debug"]));
    await vscode.commands.executeCommand("anvil.test");
    await waitFor("anvil test", () => hasCall(["test", "-p", "debug"]));
    await vscode.commands.executeCommand("anvil.clean");
    await waitFor("anvil clean", () => hasCall(["clean", "-p", "debug"]));
  });

  test("a launch configuration becomes cppvsdbg with the run's program, arguments, folder and environment", async () => {
    const anvil = await api();
    const resolved = await anvil.resolveDebugConfiguration({ type: "anvil", request: "launch", name: "Debug game", build: false });
    assert.ok(resolved);
    assert.strictEqual(resolved.type, "cppvsdbg");
    assert.strictEqual(resolved.request, "launch");
    assert.strictEqual(resolved.name, "Debug game");
    assert.strictEqual(resolved.program, `${ROOT}/out/debug/game${exe}`);
    assert.deepStrictEqual(resolved.args, ["--level", "1"]);
    assert.strictEqual(resolved.cwd, ROOT);
    assert.deepStrictEqual(resolved.environment, [{ name: "GAME_DATA", value: `${ROOT}/data` }]);
    assert.strictEqual(resolved.console, "integratedTerminal");
    assert.strictEqual(resolved.target, undefined, "anvil's own keys do not pass on");
  });

  test("launch configuration overrides and pass-through keys", async () => {
    const anvil = await api();
    const resolved = await anvil.resolveDebugConfiguration({
      type: "anvil",
      request: "launch",
      name: "Tool",
      target: "tool",
      build: false,
      args: ["a b"],
      cwd: "C:/elsewhere",
      env: { EXTRA: "1" },
      visualizerFile: "my.natvis",
      stopAtEntry: true,
    });
    assert.ok(resolved);
    assert.strictEqual(resolved.program, `${ROOT}/out/debug/tool${exe}`);
    assert.deepStrictEqual(resolved.args, ["a b"]);
    assert.strictEqual(resolved.cwd, "C:/elsewhere");
    assert.deepStrictEqual(resolved.environment, [{ name: "EXTRA", value: "1" }]);
    assert.strictEqual(resolved.visualizerFile, "my.natvis");
    assert.strictEqual(resolved.stopAtEntry, true);

    const gui = await anvil.resolveDebugConfiguration({ type: "anvil", request: "launch", name: "Plugin", target: "dep/plugin", build: false });
    assert.strictEqual(gui?.console, "internalConsole", "a window program gets no terminal");

    const library = await anvil.resolveDebugConfiguration({ type: "anvil", request: "launch", name: "Core", target: "core", build: false });
    assert.strictEqual(library, undefined, "a library cannot be debugged");
  });

  test("a test's launch configuration debugs its program with the test's arguments", async () => {
    const anvil = await api();
    const resolved = await anvil.resolveDebugConfiguration({ type: "anvil", request: "launch", name: "smoke", test: "smoke", build: false });
    assert.ok(resolved);
    assert.strictEqual(resolved.program, `${ROOT}/out/debug/game${exe}`);
    assert.deepStrictEqual(resolved.args, ["--smoke"]);
    assert.strictEqual(resolved.cwd, ROOT);
    assert.deepStrictEqual(resolved.environment, [{ name: "GAME_DATA", value: `${ROOT}/data` }]);

    const notOurs = await anvil.resolveDebugConfiguration({ type: "anvil", request: "launch", name: "lint", test: "dep/lint", build: false });
    assert.strictEqual(notOurs, undefined, "a test that runs python has nothing to debug");
  });

  test("an empty configuration (F5, no launch.json) debugs the selected target", async () => {
    const anvil = await api();
    await anvil.project()!.setTarget("tool");
    try {
      const resolved = await anvil.resolveDebugConfiguration({ type: "", request: "", name: "" });
      assert.ok(resolved);
      assert.strictEqual(resolved.program, `${ROOT}/out/debug/tool${exe}`);
      assert.ok(hasCall(["build", "tool", "-p", "debug"]), "F5 builds the target first");
    } finally {
      await anvil.project()!.setTarget(undefined);
    }
  });

  test("dynamic launch configurations: one per program and per test of a program", async () => {
    const anvil = await api();
    const names = anvil.debugConfigurations().map((configuration) => configuration.name);
    assert.deepStrictEqual(names, [
      "Anvil: game",
      "Anvil: tool",
      "Anvil: dep/plugin",
      "Anvil test: smoke",
      "Anvil test: fails_on_purpose",
      "Anvil test: from_tests_file",
    ]);
  });

  test("a failed build stops the launch and puts the compiler's error on the source file", async () => {
    const anvil = await api();
    const marker = path.join(PROJECT, "fail-build");
    fs.writeFileSync(marker, "");
    try {
      const resolved = await anvil.resolveDebugConfiguration({ type: "anvil", request: "launch", name: "game", target: "game" });
      assert.strictEqual(resolved, undefined);
      const source = vscode.Uri.file(path.join(PROJECT, "src", "main.cpp"));
      const diagnostics = await waitFor("the compiler error in Problems", () => {
        const found = vscode.languages.getDiagnostics(source);
        return found.length > 0 ? found : undefined;
      });
      assert.match(diagnostics[0].message, /undeclared identifier/);
      assert.strictEqual(diagnostics[0].range.start.line, 2);
      assert.strictEqual(diagnostics[0].severity, vscode.DiagnosticSeverity.Error);
    } finally {
      fs.rmSync(marker, { force: true });
    }
  });

  test("switching profile re-plans: tasks and the program path follow", async () => {
    const anvil = await api();
    const project = anvil.project()!;
    await project.setProfile("release");
    try {
      assert.strictEqual(project.model?.profile, "release");
      assert.strictEqual(project.model?.out_dir, `${ROOT}/out/release`);
      const build = (await vscode.tasks.fetchTasks({ type: "anvil" })).find((task) => task.name === "build")!;
      assert.deepStrictEqual(processArgs(build), ["build", "-p", "release"]);
      const resolved = await anvil.resolveDebugConfiguration({ type: "anvil", request: "launch", name: "game", build: false });
      assert.strictEqual(resolved?.program, `${ROOT}/out/release/game${exe}`);

      // A launch configuration may name its own profile
      const shipping = await anvil.resolveDebugConfiguration({ type: "anvil", request: "launch", name: "game", profile: "shipping", build: false });
      assert.strictEqual(shipping?.program, `${ROOT}/out/shipping/game${exe}`);
      assert.strictEqual(project.profile, "release", "the selection is unchanged");
    } finally {
      await project.setProfile(undefined);
    }
    assert.strictEqual(project.model?.profile, "debug");
  });

  test("switching toolchain: gcc gets cppdbg with gdb and the gcc problem matcher", async () => {
    const anvil = await api();
    const project = anvil.project()!;
    await project.setToolchain("gcc");
    try {
      assert.strictEqual(project.model?.toolchain.abi, "gnu");
      const build = (await vscode.tasks.fetchTasks({ type: "anvil" })).find((task) => task.name === "build")!;
      assert.deepStrictEqual(processArgs(build), ["build", "-p", "debug", "--toolchain", "gcc"]);
      assert.deepStrictEqual(build.problemMatchers, ["$anvil-gcc"]);

      const resolved = await anvil.resolveDebugConfiguration({ type: "anvil", request: "launch", name: "game", build: false });
      assert.ok(resolved);
      assert.strictEqual(resolved.type, "cppdbg");
      assert.strictEqual(resolved.MIMode, "gdb");
      assert.strictEqual(resolved.miDebuggerPath, "C:/fake/bin/gdb.exe");
      assert.strictEqual(resolved.externalConsole, false);

      await project.setToolchain("clang");
      const lldb = await anvil.resolveDebugConfiguration({ type: "anvil", request: "launch", name: "game", build: false });
      assert.strictEqual(lldb?.MIMode, "lldb");
    } finally {
      await project.setToolchain(undefined);
    }
    assert.strictEqual(project.model?.toolchain.family, "msvc");
  });

  test("Test Explorer lists the tests, grouped by package (their places come from anvil lsp, which the fake has not)", async () => {
    const anvil = await api();
    const items = anvil.tests.controller.items;
    const top: string[] = [];
    items.forEach((item) => top.push(item.id));
    assert.deepStrictEqual(top.sort(), ["fails_on_purpose", "from_tests_file", "package:dep", "smoke"]);
    assert.ok(items.get("package:dep")!.children.get("dep/lint"));

    assert.strictEqual(items.get("smoke")!.range, undefined);
  });

  test("running tests reports passes, failures with output, and runs only what was asked", async () => {
    const anvil = await api();
    const tests = anvil.tests;
    clearFakeCalls();
    const source = new vscode.CancellationTokenSource();
    const smoke = tests.findItem("smoke")!;
    const failing = tests.findItem("fails_on_purpose")!;
    await tests.run(new vscode.TestRunRequest([smoke, failing]), source.token);

    assert.strictEqual(tests.outcomes.get("smoke")?.outcome, "passed");
    const failed = tests.outcomes.get("fails_on_purpose");
    assert.strictEqual(failed?.outcome, "failed");
    assert.match(failed!.message, /exit code 3/);
    assert.match(failed!.message, /output of fails_on_purpose & more/);

    const testCalls = fakeCalls().filter((call) => call[0] === "test");
    assert.deepStrictEqual(testCalls.map((call) => call[1]).sort(), ["fails_on_purpose", "smoke"]);
    assert.ok(testCalls.every((call) => call.includes("--junit") && call.includes("-p")));

    // Everything: one run, no filter
    clearFakeCalls();
    await tests.run(new vscode.TestRunRequest(), source.token);
    const all = fakeCalls().filter((call) => call[0] === "test");
    assert.strictEqual(all.length, 1);
    assert.strictEqual(all[0][1], "-p");
    assert.strictEqual(tests.outcomes.get("dep/lint")?.outcome, "passed");

    // A label's run profile runs only the tests carrying it
    assert.deepStrictEqual(smoke.tags.map((tag) => tag.id), ["fast"]);
    clearFakeCalls();
    const fast = tests.labelProfiles.get("fast")!;
    await tests.run(new vscode.TestRunRequest(undefined, undefined, fast), source.token);
    assert.deepStrictEqual(fakeCalls().filter((call) => call[0] === "test").map((call) => call[1]), ["smoke"]);
  });

  test("problem patterns read cl, clang-cl, gcc and clang diagnostics", () => {
    const patterns = vscode.extensions.getExtension("anvil.anvil")!.packageJSON.contributes.problemPatterns as { name: string; regexp: string }[];
    const msvc = new RegExp(patterns.find((pattern) => pattern.name === "anvil-msvc")!.regexp);
    const gcc = new RegExp(patterns.find((pattern) => pattern.name === "anvil-gcc")!.regexp);

    const cl = msvc.exec("C:\\p\\src\\sub\\bad.cpp(1): error C2065: 'x': undeclared identifier");
    assert.deepStrictEqual(cl?.slice(1), ["C:\\p\\src\\sub\\bad.cpp", "1", "error", "C2065", "'x': undeclared identifier"]);
    const clangCl = msvc.exec("C:\\p\\src\\sub/h.h(1,18): error: use of undeclared identifier 'y'");
    assert.deepStrictEqual(clangCl?.slice(1), ["C:\\p\\src\\sub/h.h", "1,18", "error", undefined, "use of undeclared identifier 'y'"]);
    assert.strictEqual(msvc.exec("C:\\p\\a.cpp(7): warning C4100: 'x': unreferenced parameter")?.[3], "warning");
    assert.strictEqual(msvc.exec("C:\\p\\a.cpp(7): fatal error C1083: Cannot open include file")?.[3], "fatal error");

    const gnu = gcc.exec("C:\\p\\src\\sub/h.h:1:18: error: 'y' was not declared in this scope");
    assert.deepStrictEqual(gnu?.slice(1), ["C:\\p\\src\\sub/h.h", "1", "18", "error", "'y' was not declared in this scope"]);
    assert.strictEqual(gcc.exec("/home/u/p/a.cpp:3:5: warning: unused variable 'z' [-Wunused-variable]")?.[1], "/home/u/p/a.cpp");

    // Anvil's own lines are not compiler diagnostics
    for (const line of ["error: Compiling src/main.cpp failed", "error: build failed (2 of 2 steps)", "[1/3] Compiling src/main.cpp"]) {
      assert.strictEqual(msvc.exec(line), null, line);
      assert.strictEqual(gcc.exec(line), null, line);
    }
  });

  test("filters run nothing unwanted", () => {
    const all = ["a", "ab", "pkg/x", "pkg/y", "other/x"];
    assert.strictEqual(chooseFilter(all, all, new Set(all)), undefined);
    assert.strictEqual(chooseFilter(["pkg/x", "pkg/y"], all, new Set(["pkg/x", "pkg/y"])), "pkg/");
    assert.strictEqual(chooseFilter(["ab"], all, new Set(["ab"])), "ab");
    // "a" also matches "ab": the filter still runs it, nothing better exists
    assert.strictEqual(chooseFilter(["a"], all, new Set(["a"])), "a");
  });

  test("JUnit parsing", () => {
    const cases = parseJunit(
      '<testsuites><testsuite name="p">\n<testcase name="a &amp; b" classname="p" time="1.5"/>\n' +
        '<testcase name="c" classname="p" time="0.1">\n<failure message="exit code 1">line &lt;1&gt;</failure>\n</testcase>\n' +
        '<testcase name="d" classname="p" time="0">\n<skipped/>\n</testcase>\n' +
        '<testcase name="e" classname="p" time="0">\n<error message="interrupted"/>\n</testcase></testsuite></testsuites>',
    );
    assert.deepStrictEqual(cases.get("a & b"), { name: "a & b", seconds: 1.5, outcome: "passed", message: "", output: "" });
    assert.strictEqual(cases.get("c")?.outcome, "failed");
    assert.strictEqual(cases.get("c")?.output, "line <1>");
    assert.strictEqual(cases.get("d")?.outcome, "skipped");
    assert.strictEqual(cases.get("e")?.outcome, "errored");
  });

  test("editing tests.toml re-reads the tests", async () => {
    const anvil = await api();
    const file = path.join(PROJECT, "tests.toml");
    const old = fs.readFileSync(file, "utf8");
    await rewrite(file, old + '\n[[test]]\nname = "added_later"\ncommand = ["{target:game}"]\n');
    await waitFor("the new test in Test Explorer", () => anvil.tests.controller.items.get("added_later"));
  });

  test("a manifest error from anvil targets --json goes to the status bar, not to Problems", async () => {
    const anvil = await api();
    const file = path.join(PROJECT, "anvil.toml");
    const old = fs.readFileSync(file, "utf8");
    await rewrite(file, old.replace('version = "0.1.0"', 'version = "0.1.0"\nbogus = 1'));
    try {
      await waitFor("the manifest error", () => anvil.project()!.error);
      assert.match(anvil.project()!.error!, /unknown field `bogus`/);
      assert.ok(anvil.project()!.model, "the last good model stays");
      assert.deepStrictEqual(vscode.languages.getDiagnostics(vscode.Uri.file(file)), [], "the language server's alone");
    } finally {
      await rewrite(file, old);
    }
    await waitFor("the error to clear", () => anvil.project()!.error === undefined);
  });

  test("an anvil without `lsp`: one message, and everything else still works", async () => {
    const anvil = await api();
    const failure = await waitFor("the language server's failure", () => anvil.languageServer.failure);
    assert.match(failure, /`anvil lsp` did not start/);
    assert.strictEqual(anvil.languageServer.running, false);

    // Another look at the project does not start it again, so no second message
    await vscode.commands.executeCommand("anvil.openManifest");
    await anvil.project()!.refresh();
    assert.strictEqual(anvil.languageServer.failure, failure);
    assert.ok(anvil.project()!.model);
  });

  test("IntelliSense: the C/C++ extension is pointed at compile_commands.json", async function () {
    if (!vscode.extensions.getExtension("ms-vscode.cpptools")) {
      this.skip();
    }
    const value = await waitFor("C_Cpp.default.compileCommands", () =>
      vscode.workspace.getConfiguration("C_Cpp").inspect("default.compileCommands")?.workspaceValue,
    );
    assert.strictEqual(value, "${workspaceFolder}/out/compile_commands.json");
  });

  test("IntelliSense: clangd is pointed at the folder of compile_commands.json", async function () {
    if (!vscode.extensions.getExtension("llvm-vs-code-extensions.vscode-clangd")) {
      this.skip();
    }
    const value = await waitFor("clangd.arguments", () => vscode.workspace.getConfiguration("clangd").inspect("arguments")?.workspaceValue);
    assert.deepStrictEqual(value, ["--compile-commands-dir=${workspaceFolder}/out"]);
  });
});
