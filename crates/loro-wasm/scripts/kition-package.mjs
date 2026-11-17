#!/usr/bin/env node
// Builds and packs @kition/loro-crdt: loro-crdt built from KitionPkg/loro (branch `kition`),
// which carries no MPL-2.0 `im` crate. The layout and exports are upstream's; only the name,
// the version, the repository URL, the NOTICE and the wasm source map link differ.
//
//   node scripts/kition-package.mjs build
//     upstream's release pipeline (scripts/build.ts release, rollup -c, scripts/post-rollup.ts),
//     with the build machine's home directory remapped out of the wasm and the
//     sourceMappingURL section (a link to upstream's loro-crdt-map package) stripped before
//     the base64 variant inlines the wasm. Then run upstream's tests (package.json `test`).
//   node scripts/kition-package.mjs pack --version 1.16.4-kition.1 --out <dir>
//     stages the built package under its new name, checks it and runs `npm pack` into <dir>.
//     It never publishes.
//
// Needs Deno 2, wasm-bindgen-cli 0.2.100 (the version Cargo.lock pins), the wasm32 target,
// Node 22 and this package's node_modules (pnpm install --filter loro-crdt).

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const NAME = "@kition/loro-crdt";
const REPOSITORY = "git+https://github.com/KitionPkg/loro.git";
const TARGETS = ["bundler", "browser", "nodejs", "web"];
const FORBIDDEN = [
  { what: "the MPL-2.0 im crate", pattern: /im-15\.|sized-chunks-|bitmaps-\d|\bim::/ },
  { what: "a link to upstream's source maps", pattern: /loro-crdt-map/ },
];
const NOTICE = `Loro (loro-crdt), built from https://github.com/KitionPkg/loro (branch kition), a copy of
https://github.com/loro-dev/loro with the MPL-2.0 \`im\` crate replaced by a permissively
licensed persistent map. Loro is MIT-licensed; see LICENSE.
`;

const README_LINE = `${NAME} is Loro built from [KitionPkg/loro](https://github.com/KitionPkg/loro) without the MPL-2.0 \`im\` crate; the API is loro-crdt's.`;

const packageDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const workspaceRoot = path.resolve(packageDir, "../..");

function run(command, args, env = {}) {
  console.log(">", command, args.join(" "));
  execFileSync(command, args, {
    cwd: packageDir,
    stdio: "inherit",
    env: { ...process.env, ...env },
  });
}

function stripCustom(wasm, section) {
  run("cargo", [
    "run", "--quiet", "--manifest-path", path.join(workspaceRoot, "Cargo.toml"),
    "-p", "loro-wasm-tools", "--", "strip-custom", wasm, wasm, section,
  ]);
}

function build() {
  const home = os.homedir();
  // rustc applies the last prefix that matches: the workspace becomes /loro, the rest of the
  // home directory (the Cargo registry and git checkouts) /build.
  const rustflags = [
    process.env.RUSTFLAGS,
    `--remap-path-prefix=${home}=/build`,
    `--remap-path-prefix=${workspaceRoot}=/loro`,
  ].filter(Boolean).join(" ");
  run("deno", ["run", "-A", "./scripts/build.ts", "release"], { RUSTFLAGS: rustflags });
  for (const target of TARGETS) {
    stripCustom(path.join(packageDir, target, "loro_wasm_bg.wasm"), "sourceMappingURL");
  }
  run("rollup", ["-c"]);
  run("deno", ["run", "-A", "./scripts/post-rollup.ts"]);
}

function files(dir) {
  return fs.readdirSync(dir, { recursive: true, withFileTypes: true })
    .filter((entry) => entry.isFile())
    .map((entry) => path.join(entry.parentPath, entry.name));
}

// The wasm the base64 variant inlines, decoded.
function inlinedWasm(source) {
  const match = /_loadWasmModule\(1, null, '([A-Za-z0-9+/=]+)'/.exec(source);
  if (!match) throw new Error("base64/index.js: no inlined wasm found");
  return Buffer.from(match[1], "base64");
}

function check(stage) {
  const home = os.homedir();
  const problems = [];
  for (const file of files(stage)) {
    const relative = path.relative(stage, file);
    if (relative.endsWith(".wasm.map")) problems.push(`${relative}: a wasm source map`);
    const bytes = fs.readFileSync(file);
    const contents = [bytes.toString("latin1")];
    if (relative === path.join("base64", "index.js")) {
      contents.push(inlinedWasm(contents[0]).toString("latin1"));
    }
    for (const text of contents) {
      if (text.includes(home)) problems.push(`${relative}: the build machine's home directory`);
      for (const { what, pattern } of FORBIDDEN) {
        if (pattern.test(text)) problems.push(`${relative}: ${what}`);
      }
    }
  }
  if (problems.length) throw new Error(`the package is not clean:\n${problems.join("\n")}`);
}

function pack(version, out) {
  const manifest = JSON.parse(fs.readFileSync(path.join(packageDir, "package.json"), "utf8"));
  if (!version.startsWith(`${manifest.version}-kition.`)) {
    throw new Error(`--version must be ${manifest.version}-kition.<n>, the upstream version it builds`);
  }
  const stage = fs.mkdtempSync(path.join(os.tmpdir(), "kition-loro-crdt-"));
  try {
    for (const entry of manifest.files) {
      const from = path.join(packageDir, entry);
      if (!fs.existsSync(from)) throw new Error(`${entry} is missing; run the build first`);
      fs.cpSync(from, path.join(stage, entry), { recursive: true });
    }
    fs.writeFileSync(path.join(stage, "NOTICE"), NOTICE);
    const readme = fs.readFileSync(path.join(stage, "README.md"), "utf8");
    fs.writeFileSync(path.join(stage, "README.md"), `> ${README_LINE}\n\n${readme}`);
    const published = {
      ...manifest,
      name: NAME,
      version,
      repository: { type: "git", url: REPOSITORY },
      files: [...manifest.files, "NOTICE"],
    };
    fs.writeFileSync(path.join(stage, "package.json"), `${JSON.stringify(published, null, 2)}\n`);
    check(stage);
    fs.mkdirSync(out, { recursive: true });
    run("npm", ["pack", stage, "--pack-destination", out]);
  } finally {
    fs.rmSync(stage, { recursive: true, force: true });
  }
}

const [command, ...rest] = process.argv.slice(2);
const option = (name) => {
  const index = rest.indexOf(`--${name}`);
  return index === -1 ? undefined : rest[index + 1];
};
if (command === "build") {
  build();
} else if (command === "pack" && option("version") && option("out")) {
  pack(option("version"), path.resolve(option("out")));
} else {
  console.error("usage: kition-package.mjs build | pack --version <x.y.z-kition.n> --out <dir>");
  process.exit(2);
}
