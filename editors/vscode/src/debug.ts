// Debugging: `type: "anvil"` launch configurations, resolved to the C/C++ extension's cppvsdbg or cppdbg.

import * as vscode from "vscode";
import { Model, Target } from "./anvil";
import { Project } from "./project";
import { TASK_TYPE, createTask, runTask } from "./tasks";

export const DEBUG_TYPE = "anvil";

/** Keys of an anvil launch configuration that do not pass on to the C/C++ debugger. */
const OWN_KEYS = new Set(["type", "request", "name", "target", "test", "profile", "toolchain", "args", "cwd", "env", "build"]);

const CPPTOOLS = "ms-vscode.cpptools";

export class AnvilDebugProvider implements vscode.DebugConfigurationProvider {
  constructor(private readonly project: () => Project | undefined) {}

  /** One configuration per program, and one per test that runs a program of the project. */
  provideDebugConfigurations(): vscode.DebugConfiguration[] {
    const project = this.project();
    if (!project?.model) {
      return [];
    }
    const configurations: vscode.DebugConfiguration[] = project.runnableTargets.map((target) => ({
      type: DEBUG_TYPE,
      request: "launch",
      name: `Anvil: ${target.qualified}`,
      target: target.qualified,
    }));
    for (const test of project.model.tests) {
      if (test.program) {
        configurations.push({ type: DEBUG_TYPE, request: "launch", name: `Anvil test: ${test.name}`, test: test.name });
      }
    }
    return configurations;
  }

  async resolveDebugConfiguration(
    _folder: vscode.WorkspaceFolder | undefined,
    configuration: vscode.DebugConfiguration,
  ): Promise<vscode.DebugConfiguration | undefined> {
    // F5 with no launch.json and Anvil picked as the debugger
    if (!configuration.type && !configuration.request && !configuration.name) {
      configuration = { type: DEBUG_TYPE, request: "launch", name: "Anvil: debug selected target" };
    }
    if (configuration.type !== DEBUG_TYPE) {
      return configuration;
    }

    const project = this.project();
    if (!project) {
      vscode.window.showErrorMessage("Anvil: no anvil.toml in this workspace.");
      return undefined;
    }
    try {
      return await resolveLaunch(project, configuration);
    } catch (error) {
      vscode.window.showErrorMessage(`Anvil: ${error instanceof Error ? error.message : String(error)}`);
      return undefined;
    }
  }
}

/** Build the program (unless `build` is false) and describe it for cppvsdbg (MSVC ABI) or cppdbg (gdb, lldb). */
async function resolveLaunch(project: Project, configuration: vscode.DebugConfiguration): Promise<vscode.DebugConfiguration | undefined> {
  if (!project.model) {
    await project.refresh();
  }
  if (!project.model) {
    throw new Error(project.error ?? "the project could not be read");
  }
  const profile: string = configuration.profile ?? project.profile;
  const toolchain: string | undefined = configuration.toolchain ?? project.toolchainChoice;
  const model = await project.modelFor(profile, toolchain);
  const launch = describeLaunch(model, configuration, project.selectedTarget?.qualified);
  if (!hasDebuggerFor(model)) {
    return undefined;
  }

  if (configuration.build !== false) {
    const task = createTask(project, {
      type: TASK_TYPE,
      command: "build",
      target: launch.target.qualified,
      profile: configuration.profile,
      toolchain: configuration.toolchain,
    });
    const code = await runTask(task);
    if (code !== 0) {
      vscode.window.showErrorMessage(`Anvil: building ${launch.target.qualified} failed; see the Problems panel and the terminal.`);
      return undefined;
    }
  }

  const resolved = debuggerConfiguration(model, configuration, launch);
  for (const [key, value] of Object.entries(configuration)) {
    if (!OWN_KEYS.has(key)) {
      resolved[key] = value;
    }
  }
  return resolved;
}

interface Launch {
  target: Target;
  program: string;
  args: string[];
  cwd: string;
  env: Record<string, string>;
}

/** What to start: the target's program with what `anvil run` (or the test) gives it, then the configuration's overrides. */
export function describeLaunch(model: Model, configuration: vscode.DebugConfiguration, selected: string | undefined): Launch {
  const findTarget = (name: string) =>
    model.targets.find((target) => target.qualified === name) ?? model.targets.find((target) => target.name === name);

  let target: Target | undefined;
  let args: string[];
  let cwd: string;
  let env: Record<string, string>;
  if (configuration.test) {
    const test = model.tests.find((candidate) => candidate.name === configuration.test);
    if (!test) {
      throw new Error(`no test named \`${configuration.test}\` (anvil test --list names them)`);
    }
    target = test.program ? findTarget(test.program) : undefined;
    if (!target) {
      throw new Error(`test \`${test.name}\` runs \`${test.command[0] ?? ""}\`, not a program of the project, so there is nothing to debug`);
    }
    args = test.command.slice(1);
    cwd = test.workdir;
    env = { ...target.env, ...test.env };
  } else {
    const name: string | undefined = configuration.target ?? selected;
    if (!name) {
      throw new Error("the project has no program to debug");
    }
    target = findTarget(name);
    if (!target) {
      throw new Error(`no target named \`${name}\` (Anvil: Select Target lists them)`);
    }
    if (!target.runnable) {
      throw new Error(`target \`${name}\` is a ${target.kind}, not a program`);
    }
    args = target.args;
    cwd = target.workdir;
    env = { ...target.env };
  }
  if (!target.output) {
    throw new Error(`target \`${target.qualified}\` has no output file to debug`);
  }

  if (Array.isArray(configuration.args)) {
    args = configuration.args.map(String);
  }
  if (typeof configuration.cwd === "string") {
    cwd = configuration.cwd;
  }
  if (configuration.env && typeof configuration.env === "object") {
    env = { ...env, ...configuration.env };
  }
  return { target, program: target.output, args, cwd, env };
}

function debuggerConfiguration(model: Model, configuration: vscode.DebugConfiguration, launch: Launch): vscode.DebugConfiguration {
  const consoleKind = vscode.workspace.getConfiguration("anvil").get<string>("debug.console", "integratedTerminal");
  const environment = Object.entries(launch.env).map(([name, value]) => ({ name, value }));
  const common = {
    request: "launch",
    name: configuration.name ?? `Anvil: ${launch.target.qualified}`,
    program: launch.program,
    args: launch.args,
    cwd: launch.cwd,
    environment,
    stopAtEntry: configuration.stopAtEntry ?? false,
  };

  if (debuggerType(model) === "cppvsdbg") {
    // A window program needs no terminal
    return { type: "cppvsdbg", ...common, console: launch.target.gui ? "internalConsole" : consoleKind };
  }

  const resolved: vscode.DebugConfiguration = {
    type: "cppdbg",
    ...common,
    MIMode: model.toolchain.debugger === "lldb" ? "lldb" : "gdb",
    externalConsole: consoleKind === "externalTerminal",
    setupCommands: [{ description: "Pretty-print values", text: "-enable-pretty-printing", ignoreFailures: true }],
  };
  if (model.toolchain.debugger_path) {
    resolved.miDebuggerPath = model.toolchain.debugger_path;
  }
  return resolved;
}

/** The C/C++ extension's debugger for the toolchain's: the Visual Studio debugger, else gdb or lldb through MI. */
function debuggerType(model: Model): string {
  return model.toolchain.debugger === "msvc" ? "cppvsdbg" : "cppdbg";
}

/** Whether an extension provides the debugger; when none does, say which extension to install. */
function hasDebuggerFor(model: Model): boolean {
  const type = debuggerType(model);
  if (hasDebugger(type)) {
    return true;
  }
  // Not awaited: the launch ends now, the message stays until the user answers
  const show = "Show C/C++ Extension";
  vscode.window
    .showErrorMessage(`Anvil: debugging needs the C/C++ extension (${CPPTOOLS}), which provides the ${type} debugger.`, show)
    .then((choice) => choice === show && vscode.commands.executeCommand("workbench.extensions.search", CPPTOOLS));
  return false;
}

function hasDebugger(type: string): boolean {
  return vscode.extensions.all.some((extension) => {
    const debuggers = extension.packageJSON?.contributes?.debuggers;
    return Array.isArray(debuggers) && debuggers.some((contributed: { type?: string }) => contributed.type === type);
  });
}
