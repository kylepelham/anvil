#!/usr/bin/env node
// A stand-in for anvil in the extension's tests: answers `targets --json` from the project's anvil.fake.json and
// logs every call to fake-calls.log in the folder it runs in.

const fs = require("fs");
const path = require("path");

const args = process.argv.slice(2);
const cwd = process.cwd();
const slash = (text) => text.split(path.sep).join("/");
const root = slash(cwd);

fs.appendFileSync(path.join(cwd, "fake-calls.log"), JSON.stringify(args) + "\n");

function option(name) {
  const index = args.indexOf(name);
  return index >= 0 ? args[index + 1] : undefined;
}

function fail(message) {
  process.stderr.write(message + "\n");
  process.exit(1);
}

const TOOLCHAINS = {
  msvc: { family: "msvc", abi: "msvc", platform: "x64-windows", debugger: "msvc", debugger_path: null },
  "clang-cl": { family: "clang-cl", abi: "msvc", platform: "x64-windows", debugger: "msvc", debugger_path: null },
  gcc: { family: "gcc", abi: "gnu", platform: "x64-windows", debugger: "gdb", debugger_path: "C:/fake/bin/gdb.exe" },
  clang: { family: "clang", abi: "gnu", platform: "x64-windows", debugger: "lldb", debugger_path: "C:/fake/bin/lldb-mi.exe" },
};

/** The model, as anvil targets --json prints it for the options given. */
function model() {
  const manifest = path.join(cwd, "anvil.toml");
  if (!fs.existsSync(manifest)) {
    fail("error: no anvil.toml here or above");
  }
  const lines = fs.readFileSync(manifest, "utf8").split(/\r?\n/);
  const bogus = lines.findIndex((line) => /^\s*bogus\b/.test(line));
  if (bogus >= 0) {
    fail(
      `error: ${root}/anvil.toml: TOML parse error at line ${bogus + 1}, column 1\n  |\n${bogus + 1} | ${lines[bogus]}\n  | ^^^^^\nunknown field \`bogus\`, expected one of \`name\`, \`version\``,
    );
  }

  const template = JSON.parse(fs.readFileSync(path.join(cwd, "anvil.fake.json"), "utf8"));
  const profile = option("-p") ?? (args.includes("-r") ? "release" : "debug");
  if (!template.profiles.includes(profile)) {
    fail(`error: unknown profile \`${profile}\`. Known: ${template.profiles.join(", ")}`);
  }
  const toolchainName = option("--toolchain") ?? template.toolchain;
  const toolchain = TOOLCHAINS[toolchainName];
  if (!toolchain) {
    fail(`error: unknown toolchain \`${toolchainName}\``);
  }
  const out = `${root}/out/${profile}`;
  const exe = process.platform === "win32" ? ".exe" : "";
  const fill = (text) => text.replace(/\{root\}/g, root).replace(/\{out\}/g, out).replace(/\{exe\}/g, exe);

  // Tests: those of the template, and every `name = "..."` of tests.toml
  const tests = template.tests.map((test) => ({ ...test, command: test.command.map(fill), workdir: fill(test.workdir) }));
  const testsFile = path.join(cwd, "tests.toml");
  if (fs.existsSync(testsFile)) {
    for (const match of fs.readFileSync(testsFile, "utf8").matchAll(/^name\s*=\s*"([^"]+)"/gm)) {
      tests.push({ name: match[1], program: "game", command: [`${out}/game${exe}`, match[1]], workdir: root });
    }
  }

  return {
    version: 1,
    root,
    profile,
    profiles: template.profiles,
    toolchain,
    out_dir: out,
    compile_commands: `${root}/out/compile_commands.json`,
    default_run: template.default_run,
    targets: template.targets.map((target) => ({
      ...target,
      output: target.output === null ? null : fill(target.output),
      workdir: fill(target.workdir),
      dir: fill(target.dir),
      args: target.args.map(fill),
      env: Object.fromEntries(Object.entries(target.env).map(([key, value]) => [key, fill(value)])),
    })),
    tests,
  };
}

const command = args[0];
if (command === "--version") {
  console.log("anvil 0.1.0-fake");
} else if (command === "schema") {
  const what = args.includes("--tests") ? "tests.toml" : "anvil.toml";
  console.log(JSON.stringify({ type: "object", description: `${what}: fake schema`, properties: { project: { type: "object" } }, additionalProperties: false }, null, 2));
} else if (command === "targets") {
  console.log(JSON.stringify(model(), null, 2));
} else if (command === "toolchains") {
  console.log("* msvc      19.51.0    C:/fake/cl.exe\n  gcc       13.2.0     C:/fake/g++.exe\n");
} else if (command === "build" || command === "run") {
  const current = model();
  if (fs.existsSync(path.join(cwd, "fail-build"))) {
    console.log("[1/2] Compiling src/main.cpp");
    console.log(`${slash(path.join(cwd, "src", "main.cpp")).replace(/\//g, "\\")}(3): error C2065: 'x': undeclared identifier`);
    console.log("error: build failed (1 of 1 steps)");
    process.exit(1);
  }
  fs.mkdirSync(path.join(cwd, "out"), { recursive: true });
  fs.writeFileSync(path.join(cwd, "out", "compile_commands.json"), "[]\n");
  console.log(`    Finished ${current.profile} [${current.toolchain.family}]`);
} else if (command === "test") {
  const current = model();
  const filter = args[1] && !args[1].startsWith("-") ? args[1] : "";
  const junit = option("--junit");
  const ran = current.tests.filter((test) => test.name.includes(filter));
  const cases = ran.map((test) => {
    const failed = test.name.includes("fail");
    console.log(`  test ${test.name} ... ${failed ? "FAILED" : "ok"}`);
    return failed
      ? `    <testcase name="${test.name}" classname="hello" time="0.010">\n      <failure message="exit code 3">output of ${test.name} &amp; more</failure>\n    </testcase>`
      : `    <testcase name="${test.name}" classname="hello" time="0.020"/>`;
  });
  if (junit) {
    fs.writeFileSync(junit, `<?xml version="1.0" encoding="UTF-8"?>\n<testsuites>\n  <testsuite name="hello">\n${cases.join("\n")}\n  </testsuite>\n</testsuites>\n`);
  }
} else if (command === "clean" || command === "convert") {
  if (command === "convert") {
    fs.writeFileSync(path.join(cwd, "anvil.toml"), '[project]\nname = "converted"\n');
  }
} else {
  fail(`error: unknown command ${command}`);
}
