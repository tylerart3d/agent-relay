import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import process from "node:process";

const targets = {
  "win32-x64": {
    bunTarget: "bun-windows-x64",
    triple: "x86_64-pc-windows-msvc",
    extension: ".exe",
  },
  "darwin-arm64": {
    bunTarget: "bun-darwin-arm64",
    triple: "aarch64-apple-darwin",
    extension: "",
  },
};

const target = targets[`${process.platform}-${process.arch}`];
if (!target) {
  throw new Error(`unsupported gateway build host: ${process.platform}-${process.arch}`);
}

const bun = resolve(
  "channel-gateway",
  "node_modules",
  "bun",
  "bin",
  process.platform === "win32" ? "bun.exe" : "bun",
);
if (!existsSync(bun)) {
  throw new Error("missing channel-gateway Bun compiler; run npm install in channel-gateway");
}

const output = resolve(
  "src-tauri",
  "binaries",
  `agent-relay-gateway-${target.triple}${target.extension}`,
);
mkdirSync(dirname(output), { recursive: true });
const stagingDirectory = mkdtempSync(resolve(tmpdir(), "agent-relay-gateway-"));
try {
  const bundle = resolve(stagingDirectory, "gateway.js");
  const bundleResult = spawnSync(
    bun,
    [
      "build",
      resolve("channel-gateway", "src", "index.ts"),
      "--target=bun",
      "--packages=bundle",
      `--outfile=${bundle}`,
    ],
    { stdio: "inherit" },
  );
  if (bundleResult.status !== 0) {
    throw new Error(`gateway bundling failed with exit code ${bundleResult.status ?? "unknown"}`);
  }

  // advanced-imessage verifies its optional gRPC peers with import.meta.resolve.
  // Bun embeds those peers, but its standalone resolver cannot see their package
  // names. The imports still fail naturally if a future bundle omits them.
  const guard = "assertPeersResolvable();";
  const bundledSource = readFileSync(bundle, "utf8");
  const guardCount = bundledSource.split(guard).length - 1;
  if (guardCount !== 1) {
    throw new Error(`expected one Photon peer-resolution guard, found ${guardCount}`);
  }
  writeFileSync(bundle, bundledSource.replace(guard, "/* peers are embedded */"));

  const result = spawnSync(
    bun,
    [
      "build",
      bundle,
      "--compile",
      `--target=${target.bunTarget}`,
      `--outfile=${output}`,
      ...(process.platform === "win32" && process.env.AGENT_RELAY_GATEWAY_SHOW_CONSOLE !== "1"
        ? ["--windows-hide-console"]
        : []),
    ],
    { stdio: "inherit" },
  );
  if (result.status !== 0) {
    throw new Error(`gateway compilation failed with exit code ${result.status ?? "unknown"}`);
  }
} finally {
  rmSync(stagingDirectory, { recursive: true, force: true });
}
if (process.platform !== "win32") chmodSync(output, 0o755);
console.log(`Staged Agent Relay channel gateway: ${output}`);
