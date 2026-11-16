// Runs the extension's tests in a downloaded VS Code (never the installed one), against copies of the fixture projects.
//
// Suites: `fake` and `convert` (always) drive the extension with test/fixtures/fake-anvil (which has no `lsp`); `lsp`
// runs the language server of a real anvil (ANVIL_LSP, else the repository's target/debug/anvil) on test/fixtures/lsp;
// `real` runs when ANVIL_REAL names an anvil that has `targets --json`, and builds and debugs test/fixtures/real with it
// (ANVIL_TEST_GCC=1 debugs a gcc build with gdb too). The C/C++, clangd and Even Better TOML extensions are installed
// into .vscode-test/extensions, the test instance's own (Even Better TOML to check it leaves anvil.toml alone).

import * as childProcess from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { downloadAndUnzipVSCode, resolveCliArgsFromVSCodeExecutablePath, runTests } from "@vscode/test-electron";

const EXTENSIONS = ["ms-vscode.cpptools", "tamasfe.even-better-toml", "llvm-vs-code-extensions.vscode-clangd"];

async function main(): Promise<void> {
  const extensionRoot = path.resolve(__dirname, "../..");
  const fixtures = path.join(extensionRoot, "test", "fixtures");
  const testRoot = path.join(extensionRoot, ".vscode-test");
  const extensionsDir = path.join(testRoot, "extensions");
  const vscodeExecutablePath = await downloadAndUnzipVSCode(process.env.ANVIL_TEST_VSCODE ?? "stable");

  // The test instance's extensions, installed once
  const [cli, ...cliArgs] = resolveCliArgsFromVSCodeExecutablePath(vscodeExecutablePath);
  for (const id of EXTENSIONS) {
    const installed = fs.existsSync(extensionsDir) && fs.readdirSync(extensionsDir).some((name) => name.toLowerCase().startsWith(id.toLowerCase() + "-"));
    if (!installed && !process.env.ANVIL_TEST_NO_EXTENSIONS) {
      const result = childProcess.spawnSync(cli, [...cliArgs, "--extensions-dir", extensionsDir, "--install-extension", id], {
        stdio: "inherit",
        shell: process.platform === "win32",
      });
      if (result.status !== 0) {
        console.warn(`Installing ${id} into the test instance failed; tests that need it are skipped.`);
      }
    }
  }

  const work = fs.mkdtempSync(path.join(os.tmpdir(), "anvil-vscode-"));
  const suites: { name: string; project: string; anvil: string }[] = [];

  // The fake suite: a project whose anvil is the fake
  fs.cpSync(path.join(fixtures, "fake-anvil"), path.join(work, "fake-anvil"), { recursive: true });
  fs.cpSync(path.join(fixtures, "hello"), path.join(work, "hello"), { recursive: true });
  const fake = path.join(work, "fake-anvil", process.platform === "win32" ? "anvil.cmd" : "anvil");
  if (process.platform !== "win32") {
    fs.chmodSync(fake, 0o755);
  }
  suites.push({ name: "fake", project: path.join(work, "hello"), anvil: fake });
  fs.cpSync(path.join(fixtures, "cmake"), path.join(work, "cmake"), { recursive: true });
  suites.push({ name: "convert", project: path.join(work, "cmake"), anvil: fake });

  // The language server suite: the anvil built in this repository unless ANVIL_LSP names one
  const built = path.resolve(extensionRoot, "../../target/debug", process.platform === "win32" ? "anvil.exe" : "anvil");
  const server = process.env.ANVIL_LSP ?? (fs.existsSync(built) ? built : undefined);
  if (server) {
    fs.cpSync(path.join(fixtures, "lsp"), path.join(work, "lsp"), { recursive: true });
    suites.push({ name: "lsp", project: path.join(work, "lsp"), anvil: path.resolve(server) });
  } else {
    console.warn(`No anvil for the lsp suite: build it (cargo build) or set ANVIL_LSP. Skipped.`);
  }

  const real = process.env.ANVIL_REAL;
  if (real) {
    fs.cpSync(path.join(fixtures, "real"), path.join(work, "real"), { recursive: true });
    suites.push({ name: "real", project: path.join(work, "real"), anvil: path.resolve(real) });
  }

  let failed = false;
  for (const suite of suites) {
    fs.mkdirSync(path.join(suite.project, ".vscode"), { recursive: true });
    fs.writeFileSync(path.join(suite.project, ".vscode", "settings.json"), JSON.stringify({ "anvil.path": suite.anvil }, null, 2));
    const userDataDir = path.join(testRoot, `user-data-${suite.name}`);
    fs.rmSync(userDataDir, { recursive: true, force: true });

    console.log(`\n=== ${suite.name} suite: ${suite.project}`);
    try {
      await runTests({
        vscodeExecutablePath,
        extensionDevelopmentPath: extensionRoot,
        extensionTestsPath: path.join(__dirname, "suite", "index"),
        extensionTestsEnv: { ANVIL_TEST_SUITE: suite.name, ANVIL_TEST_PROJECT: suite.project },
        launchArgs: [suite.project, "--extensions-dir", extensionsDir, "--user-data-dir", userDataDir, "--disable-workspace-trust", "--skip-welcome", "--skip-release-notes"],
      });
    } catch (error) {
      console.error(`${suite.name} suite failed: ${error}`);
      failed = true;
    }
  }
  fs.rmSync(work, { recursive: true, force: true, maxRetries: 3 });
  process.exit(failed ? 1 : 0);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
