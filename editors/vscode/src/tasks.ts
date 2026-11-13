// Tasks of type `anvil`: build, run, test and clean, with problem matchers for the toolchain's diagnostics.

import * as vscode from "vscode";
import { quoteCommand, shellArgs } from "./anvil";
import { Project } from "./project";

export const TASK_TYPE = "anvil";
export const TASK_SOURCE = "anvil";

export type AnvilCommand = "build" | "run" | "test" | "clean";

export interface AnvilTaskDefinition extends vscode.TaskDefinition {
  command: AnvilCommand;
  target?: string;
  profile?: string;
  toolchain?: string;
  args?: string[];
}

const MSVC_STYLE = ["msvc", "clang-cl"];

export class AnvilTaskProvider implements vscode.TaskProvider {
  constructor(private readonly project: () => Project | undefined) {}

  provideTasks(): vscode.Task[] {
    const project = this.project();
    if (!project) {
      return [];
    }

    const tasks = [createTask(project, { type: TASK_TYPE, command: "build" })];
    for (const target of project.model?.targets ?? []) {
      tasks.push(createTask(project, { type: TASK_TYPE, command: "build", target: target.qualified }));
    }
    for (const target of project.runnableTargets) {
      tasks.push(createTask(project, { type: TASK_TYPE, command: "run", target: target.qualified }));
    }
    tasks.push(createTask(project, { type: TASK_TYPE, command: "test" }));
    tasks.push(createTask(project, { type: TASK_TYPE, command: "clean" }));
    return tasks;
  }

  resolveTask(task: vscode.Task): vscode.Task | undefined {
    const project = this.project();
    const definition = task.definition as AnvilTaskDefinition;
    if (!project || definition.type !== TASK_TYPE || !definition.command) {
      return undefined;
    }
    // A task from tasks.json must keep its own definition object
    return createTask(project, definition, task.name);
  }
}

/** The task for an anvil command; profile and toolchain default to the status bar's choice. */
export function createTask(project: Project, definition: AnvilTaskDefinition, name?: string): vscode.Task {
  const profile = definition.profile ?? project.profile;
  const toolchain = definition.toolchain ?? project.toolchainChoice;

  // anvil <command> [target] -p <profile> [--toolchain <name>] [args]
  const args: string[] = [definition.command];
  let target = definition.target;
  if (definition.command === "run" && !target) {
    target = project.selectedTarget?.qualified;
  }
  if (target && definition.command !== "clean") {
    args.push(target);
  }
  args.push(...project.planArgs(profile, toolchain));
  args.push(...(definition.args ?? []));

  const label = name ?? [definition.command, definition.target].filter(Boolean).join(" ");
  const anvil = project.anvil ?? "anvil";
  const execution = new vscode.ProcessExecution(quoteCommand(anvil), shellArgs(anvil, args), {
    cwd: project.manifestDir,
  });
  const task = new vscode.Task(definition, project.folder, label, TASK_SOURCE, execution, problemMatchers(project, toolchain));
  task.detail = `anvil ${args.join(" ")}`;

  // `anvil: build` is the default build task and `anvil: test` the default test task, unless tasks.json names others
  if (definition.command === "build" && !definition.target) {
    task.group = defaultGroup("build", "Build");
  } else if (definition.command === "build") {
    task.group = vscode.TaskGroup.Build;
  } else if (definition.command === "test" && !definition.target) {
    task.group = defaultGroup("test", "Test");
  } else if (definition.command === "test") {
    task.group = vscode.TaskGroup.Test;
  } else if (definition.command === "clean") {
    task.group = vscode.TaskGroup.Clean;
  }

  task.presentationOptions = {
    clear: true,
    focus: definition.command === "run",
    reveal: vscode.TaskRevealKind.Always,
  };
  return task;
}

/** Run a task and wait for its process to end: its exit code, or undefined when it did not start. */
export function runTask(task: vscode.Task): Promise<number | undefined> {
  return new Promise((resolve, reject) => {
    let execution: vscode.TaskExecution | undefined;
    let finished = false;
    // Processes that ended before executeTask said which execution is ours
    const early: { execution: vscode.TaskExecution; code: number | undefined }[] = [];
    const subscriptions: vscode.Disposable[] = [];
    const finish = (code: number | undefined) => {
      if (!finished) {
        finished = true;
        subscriptions.forEach((subscription) => subscription.dispose());
        resolve(code);
      }
    };
    subscriptions.push(
      vscode.tasks.onDidEndTaskProcess((event) => {
        if (!execution) {
          early.push({ execution: event.execution, code: event.exitCode });
        } else if (event.execution === execution) {
          finish(event.exitCode);
        }
      }),
      vscode.tasks.onDidEndTask((event) => {
        // A task whose process never started ends without an exit code
        if (event.execution === execution) {
          setTimeout(() => finish(undefined), 500);
        }
      }),
    );
    vscode.tasks.executeTask(task).then(
      (started) => {
        execution = started;
        const ended = early.find((candidate) => candidate.execution === started);
        if (ended) {
          finish(ended.code);
        }
      },
      (error) => {
        subscriptions.forEach((subscription) => subscription.dispose());
        reject(error);
      },
    );
  });
}

/**
 * Problem matchers for how the compiler prints diagnostics: cl and clang-cl `file(line,col): error C...:`, gcc and clang
 * `file:line:col: error:` (clang does so with the MSVC ABI too). Anvil passes the compilers absolute paths.
 */
function problemMatchers(project: Project, toolchain: string | undefined): string[] {
  const family = toolchain ?? project.model?.toolchain.family;
  if (!family) {
    return ["$anvil-msvc", "$anvil-gcc"];
  }
  return MSVC_STYLE.includes(family) ? ["$anvil-msvc"] : ["$anvil-gcc"];
}

/** A task group that is its kind's default; the API's groups are never default, the constructor is public at run time. */
function defaultGroup(id: string, label: string): vscode.TaskGroup {
  const group = new (vscode.TaskGroup as unknown as new (id: string, label: string) => vscode.TaskGroup)(id, label);
  (group as { isDefault?: boolean }).isDefault = true;
  return group;
}
