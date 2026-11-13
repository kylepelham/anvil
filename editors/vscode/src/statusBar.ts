// Status bar: profile, toolchain and target, each a click away from a picker.

import * as vscode from "vscode";
import { runAnvil } from "./anvil";
import { Project, TOOLCHAINS } from "./project";

export class StatusBar implements vscode.Disposable {
  private readonly profile = vscode.window.createStatusBarItem("anvil.profile", vscode.StatusBarAlignment.Left, 50);
  private readonly toolchain = vscode.window.createStatusBarItem("anvil.toolchain", vscode.StatusBarAlignment.Left, 49);
  private readonly target = vscode.window.createStatusBarItem("anvil.target", vscode.StatusBarAlignment.Left, 48);

  constructor(private readonly project: () => Project | undefined) {
    this.profile.name = "Anvil Profile";
    this.toolchain.name = "Anvil Toolchain";
    this.target.name = "Anvil Target";
    this.profile.command = "anvil.selectProfile";
    this.toolchain.command = "anvil.selectToolchain";
    this.target.command = "anvil.selectTarget";
  }

  dispose(): void {
    this.profile.dispose();
    this.toolchain.dispose();
    this.target.dispose();
  }

  update(): void {
    const project = this.project();
    if (!project) {
      this.profile.hide();
      this.toolchain.hide();
      this.target.hide();
      return;
    }

    const model = project.model;
    if (project.error) {
      this.profile.text = "$(error) Anvil";
      this.profile.tooltip = `${project.error}\n\nClick to open anvil.toml.`;
      this.profile.command = "anvil.openManifest";
      this.profile.backgroundColor = new vscode.ThemeColor("statusBarItem.errorBackground");
    } else {
      this.profile.text = `$(gear) ${project.profile}`;
      this.profile.tooltip = "Anvil profile: click to change";
      this.profile.command = "anvil.selectProfile";
      this.profile.backgroundColor = undefined;
    }
    this.profile.show();

    if (model) {
      const toolchain = model.toolchain;
      this.toolchain.text = `$(tools) ${toolchain.family}`;
      this.toolchain.tooltip = `Anvil toolchain: ${toolchain.family} (${toolchain.abi} ABI, ${toolchain.platform})${
        project.toolchainChoice ? "" : ", the project's default"
      }: click to change`;
      this.toolchain.show();

      const selected = project.selectedTarget;
      this.target.text = `$(target) ${selected?.qualified ?? "no program"}`;
      this.target.tooltip = selected ? `Anvil target for Run and Debug (F5): ${selected.output ?? selected.qualified}` : "The project has no program to run";
      this.target.show();
    } else {
      this.toolchain.hide();
      this.target.hide();
    }
  }
}

export async function pickProfile(project: Project): Promise<void> {
  const profiles = project.model?.profiles ?? ["debug", "release"];
  const picked = await vscode.window.showQuickPick(
    profiles.map((profile) => ({ label: profile, description: profile === project.profile ? "current" : undefined })),
    { placeHolder: "Profile to build, run and debug with" },
  );
  if (picked) {
    await project.setProfile(picked.label);
  }
}

export async function pickToolchain(project: Project): Promise<void> {
  // anvil toolchains lists what is installed: "* msvc  19.51  C:/.../cl.exe"
  let installed: { name: string; version: string; path: string }[] = [];
  const anvil = project.anvil;
  if (anvil) {
    const result = await runAnvil(anvil, ["toolchains"], project.manifestDir).catch(() => undefined);
    for (const line of result?.code === 0 ? result.stdout.split(/\r?\n/) : []) {
      const match = /^[* ]\s*(msvc|clang-cl|clang|gcc)\s+(\S+)\s+(.*)$/.exec(line);
      if (match) {
        installed.push({ name: match[1], version: match[2], path: match[3].trim() });
      }
    }
  }
  if (installed.length === 0) {
    installed = TOOLCHAINS.map((name) => ({ name, version: "", path: "" }));
  }

  const projectDefault = "Project default";
  const items: vscode.QuickPickItem[] = [
    { label: projectDefault, description: project.toolchainChoice ? undefined : "current", detail: "The project's toolchain, else the machine's default" },
    ...installed.map((toolchain) => ({
      label: toolchain.name,
      description: [toolchain.version, toolchain.name === project.toolchainChoice ? "current" : undefined].filter(Boolean).join("  "),
      detail: toolchain.path || undefined,
    })),
  ];
  const picked = await vscode.window.showQuickPick(items, { placeHolder: "Toolchain to build with" });
  if (picked) {
    await project.setToolchain(picked.label === projectDefault ? undefined : picked.label);
  }
}

export async function pickTarget(project: Project): Promise<void> {
  const selected = project.selectedTarget;
  const items = project.runnableTargets.map((target) => ({
    label: target.qualified,
    description: [target.kind, target === selected ? "current" : undefined].filter(Boolean).join("  "),
    detail: target.output ?? undefined,
  }));
  if (items.length === 0) {
    vscode.window.showInformationMessage("Anvil: the project has no program to run.");
    return;
  }
  const picked = await vscode.window.showQuickPick(items, { placeHolder: "Target to run and debug (F5)" });
  if (picked) {
    await project.setTarget(picked.label);
  }
}
