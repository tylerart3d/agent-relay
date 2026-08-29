import { chmodSync, copyFileSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const manifest = join(root, "src-tauri", "Cargo.toml");

function run(command, args) {
  const result = spawnSync(command, args, {
    cwd: root,
    encoding: "utf8",
    stdio: ["inherit", "pipe", "inherit"],
  });
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
  return result.stdout;
}

const rustc = process.platform === "win32" ? "rustc.exe" : "rustc";
const cargo = process.platform === "win32" ? "cargo.exe" : "cargo";
const hostLine = run(rustc, ["-vV"])
  .split(/\r?\n/)
  .find((line) => line.startsWith("host: "));
if (!hostLine) {
  throw new Error("rustc did not report its host target");
}
const target = hostLine.slice("host: ".length).trim();

const extension = process.platform === "win32" ? ".exe" : "";
const destination = join(
  root,
  "src-tauri",
  "binaries",
  `agentrelayctl-${target}${extension}`,
);
mkdirSync(dirname(destination), { recursive: true });
// Tauri validates every declared sidecar before Cargo compiles any binary in
// the package. Seed the target-specific path on a clean checkout, then replace
// it with the newly built CLI before the installer bundling phase begins.
if (!existsSync(destination)) {
  writeFileSync(destination, "");
}
run(cargo, ["build", "--manifest-path", manifest, "--release", "--bin", "agentrelayctl"]);

const source = join(root, "src-tauri", "target", "release", `agentrelayctl${extension}`);
copyFileSync(source, destination);
if (process.platform !== "win32") {
  chmodSync(destination, 0o755);
}
console.log(`Staged Agent Relay CLI: ${destination}`);
