// Anvil for VS Code: everything comes from `anvil targets --json`; nothing is generated into the project.

import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import { findAnvil, quoteCommand, shellArgs } from "./anvil";
import { SnippetCompletions } from "./completion";
import { AnvilDebugProvider, DEBUG_TYPE } from "./debug";
import { IntelliSense } from "./intellisense";
import { Project, findManifestDir } from "./project";
import { pointSchemaAssociations, updateSchemas } from "./schema";
import { StatusBar, pickProfile, pickTarget, pickToolchain } from "./statusBar";
import { AnvilCommand, AnvilTaskProvider, AnvilTaskDefinition, TASK_SOURCE, TASK_TYPE, createTask, runTask } from "./tasks";
import { AnvilTests } from "./testing";

const REFRESH_DELAY_MS = 300;

/** What the extension hands to other extensions and to its tests. */
export interface AnvilApi {
  project(): Project | undefined;
  /** Resolves once the workspace has been searched for a project and its model read. */
  ready: Promise<void>;
  /** The C/C++ debugger configuration an anvil launch configuration becomes (building first unless `build` is false). */
  resolveDebugConfiguration(configuration: vscode.DebugConfiguration): Promise<vscode.DebugConfiguration | undefined>;
  /** The launch configurations listed in Run and Debug. */
  debugConfigurations(): vscode.DebugConfiguration[];
  tests: AnvilTests;
}

export function activate(context: vscode.ExtensionContext): AnvilApi {
  // Before Even Better TOML starts and reads the schema URLs
  pointSchemaAssociations(context);

  const output = vscode.window.createOutputChannel("Anvil");
  const diagnostics = vscode.languages.createDiagnosticCollection("anvil");
  let project: Project | undefined;
  const current = () => project;

  const statusBar = new StatusBar(current);
  const tests = new AnvilTests(current);
  const intelliSense = new IntelliSense(context);
  const debugProvider = new AnvilDebugProvider(current);
  context.subscriptions.push(
    output,
    diagnostics,
    statusBar,
    tests,
    vscode.tasks.registerTaskProvider(TASK_TYPE, new AnvilTaskProvider(current)),
    vscode.debug.registerDebugConfigurationProvider(DEBUG_TYPE, debugProvider, vscode.DebugConfigurationProviderTriggerKind.Initial),
    vscode.debug.registerDebugConfigurationProvider(DEBUG_TYPE, debugProvider, vscode.DebugConfigurationProviderTriggerKind.Dynamic),
    vscode.languages.registerCompletionItemProvider(
      [{ scheme: "file", pattern: "**/anvil.toml" }, { scheme: "file", pattern: "**/tests.toml" }],
      new SnippetCompletions(),
      "[",
    ),
  );

  const onProjectChanged = async () => {
    statusBar.update();
    await tests.update();
    if (project) {
      await intelliSense.configure(project);
    }
  };

  /** Find the workspace's anvil.toml and read its model; the first folder that has one wins. */
  const discover = async () => {
    let found: Project | undefined;
    for (const folder of vscode.workspace.workspaceFolders ?? []) {
      const manifestDir = await findManifestDir(folder);
      if (manifestDir) {
        if (project && project.folder === folder && project.manifestDir === manifestDir) {
          found = project;
        } else {
          found = new Project(context, folder, manifestDir, output, diagnostics);
          found.onDidChange(onProjectChanged);
        }
        break;
      }
    }
    if (found !== project) {
      project?.dispose();
      diagnostics.clear();
      project = found;
    }
    await vscode.commands.executeCommand("setContext", "anvil.active", project !== undefined);
    if (project) {
      await project.refresh();
      const anvil = project.anvil;
      if (anvil) {
        updateSchemas(context, anvil, output).catch((error) => output.appendLine(`Writing the schemas failed: ${error}`));
      }
    } else {
      await onProjectChanged();
    }
  };

  // Manifest edits re-read the model, debounced; a new or deleted anvil.toml may change the project
  let timer: NodeJS.Timeout | undefined;
  let rediscover = false;
  const scheduleRefresh = (uri: vscode.Uri, created: boolean) => {
    if (created || !project || path.basename(uri.fsPath) === "anvil.toml" && !fs.existsSync(path.join(project.manifestDir, "anvil.toml"))) {
      rediscover = true;
    }
    clearTimeout(timer);
    timer = setTimeout(() => {
      const again = rediscover;
      rediscover = false;
      (again ? discover() : project?.refresh())?.catch((error) => output.appendLine(String(error)));
    }, REFRESH_DELAY_MS);
  };
  const watcher = vscode.workspace.createFileSystemWatcher("**/{anvil.toml,tests.toml}");
  context.subscriptions.push(
    watcher,
    watcher.onDidChange((uri) => scheduleRefresh(uri, false)),
    watcher.onDidCreate((uri) => scheduleRefresh(uri, true)),
    watcher.onDidDelete((uri) => scheduleRefresh(uri, true)),
    vscode.workspace.onDidChangeWorkspaceFolders(() => discover()),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration("anvil.path")) {
        discover();
      }
    }),
    vscode.tasks.onDidEndTaskProcess((event) => {
      const definition = event.execution.task.definition as AnvilTaskDefinition;
      if (project && definition.type === TASK_TYPE && definition.command !== "clean") {
        intelliSense.afterBuild(project);
      }
    }),
  );

  registerCommands(context, current, discover, output);
  const ready = discover().catch((error) => output.appendLine(`Anvil: ${error}`));
  return {
    project: current,
    ready,
    resolveDebugConfiguration: (configuration) => debugProvider.resolveDebugConfiguration(undefined, configuration),
    debugConfigurations: () => debugProvider.provideDebugConfigurations(),
    tests,
  };
}

export function deactivate(): void {}

function registerCommands(
  context: vscode.ExtensionContext,
  current: () => Project | undefined,
  discover: () => Promise<void>,
  output: vscode.OutputChannel,
): void {
  /** Run a command that needs the project; errors become messages. */
  const withProject = (action: (project: Project) => unknown) => async () => {
    const project = current();
    if (!project) {
      vscode.window.showErrorMessage("Anvil: no anvil.toml in this workspace (Anvil: Convert CMake Project makes one from a CMakeLists.txt).");
      return;
    }
    try {
      await action(project);
    } catch (error) {
      vscode.window.showErrorMessage(`Anvil: ${error instanceof Error ? error.message : String(error)}`);
    }
  };
  const execute = (project: Project, definition: { command: AnvilCommand; target?: string }) =>
    vscode.tasks.executeTask(createTask(project, { type: TASK_TYPE, ...definition }));

  const commands: Record<string, (...args: unknown[]) => unknown> = {
    "anvil.build": withProject((project) => execute(project, { command: "build" })),
    "anvil.buildTarget": withProject((project) => {
      const target = project.selectedTarget;
      return execute(project, { command: "build", target: target?.qualified });
    }),
    "anvil.run": withProject((project) => {
      if (!project.selectedTarget) {
        throw new Error("the project has no program to run");
      }
      return execute(project, { command: "run" });
    }),
    "anvil.test": withProject((project) => execute(project, { command: "test" })),
    "anvil.clean": withProject((project) => execute(project, { command: "clean" })),
    "anvil.debugTarget": withProject(async (project) => {
      if (!project.model) {
        await project.refresh();
      }
      const target = project.selectedTarget;
      if (!target) {
        throw new Error(project.error ?? "the project has no program to debug");
      }
      await vscode.debug.startDebugging(project.folder, {
        type: DEBUG_TYPE,
        request: "launch",
        name: `Anvil: ${target.qualified}`,
        target: target.qualified,
      });
    }),
    "anvil.selectProfile": withProject(pickProfile),
    "anvil.selectToolchain": withProject(pickToolchain),
    "anvil.selectTarget": withProject(pickTarget),
    "anvil.refresh": async () => {
      await discover();
      const anvil = current()?.anvil;
      if (anvil) {
        await updateSchemas(context, anvil, output);
      }
    },
    "anvil.openManifest": withProject(async (project) => {
      const document = await vscode.workspace.openTextDocument(path.join(project.root, "anvil.toml"));
      await vscode.window.showTextDocument(document);
    }),
    "anvil.convert": () => convert(discover),
  };
  for (const [name, handler] of Object.entries(commands)) {
    context.subscriptions.push(vscode.commands.registerCommand(name, handler));
  }
}

/** `anvil convert` in a folder with a CMakeLists.txt, in a terminal; then open the anvil.toml it wrote. */
async function convert(discover: () => Promise<void>): Promise<void> {
  const folders = (vscode.workspace.workspaceFolders ?? []).filter((folder) => fs.existsSync(path.join(folder.uri.fsPath, "CMakeLists.txt")));
  if (folders.length === 0) {
    vscode.window.showErrorMessage("Anvil: no workspace folder has a CMakeLists.txt to convert.");
    return;
  }
  const folder = folders.length === 1 ? folders[0] : await vscode.window.showWorkspaceFolderPick({ placeHolder: "Folder to convert" });
  if (!folder) {
    return;
  }
  const anvil = findAnvil(folder);
  if (!anvil) {
    vscode.window.showErrorMessage("Anvil: anvil was not found: set `anvil.path`, or put anvil on PATH.");
    return;
  }

  const args = ["convert"];
  const manifest = path.join(folder.uri.fsPath, "anvil.toml");
  if (fs.existsSync(manifest)) {
    const overwrite = "Overwrite";
    const choice = await vscode.window.showWarningMessage(`${folder.name} already has an anvil.toml.`, { modal: true }, overwrite);
    if (choice !== overwrite) {
      return;
    }
    args.push("--force");
  }

  const execution = new vscode.ProcessExecution(quoteCommand(anvil), shellArgs(anvil, args), { cwd: folder.uri.fsPath });
  const task = new vscode.Task({ type: TASK_TYPE, command: "convert" }, folder, "convert", TASK_SOURCE, execution, []);
  task.presentationOptions = { clear: true, reveal: vscode.TaskRevealKind.Always };
  const code = await runTask(task);
  if (code === 0 && fs.existsSync(manifest)) {
    await discover();
    await vscode.window.showTextDocument(await vscode.workspace.openTextDocument(manifest));
  } else {
    vscode.window.showErrorMessage("Anvil: the conversion failed; see the terminal.");
  }
}
