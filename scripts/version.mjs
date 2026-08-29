import { readFile, writeFile } from "node:fs/promises";
import process from "node:process";

const files = {
  package: "package.json",
  packageLock: "package-lock.json",
  cargo: "src-tauri/Cargo.toml",
  cargoLock: "src-tauri/Cargo.lock",
  tauri: "src-tauri/tauri.conf.json",
  gatewayPackage: "channel-gateway/package.json",
  gatewayPackageLock: "channel-gateway/package-lock.json",
  readme: "README.md",
  landingPage: "index.html",
};

const semverPattern = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

function parseVersion(value) {
  const match = semverPattern.exec(value);
  if (!match) throw new Error(`invalid semantic version: ${value}`);
  return match.slice(1).map(Number);
}

function replaceOne(source, pattern, replacement, label) {
  const matches = [...source.matchAll(new RegExp(pattern.source, `${pattern.flags}g`))];
  if (matches.length !== 1) {
    throw new Error(`expected one ${label} version entry, found ${matches.length}`);
  }
  return source.replace(pattern, replacement);
}

function extractVersions(contents) {
  const packageJson = JSON.parse(contents.package);
  const packageLock = JSON.parse(contents.packageLock);
  const tauri = JSON.parse(contents.tauri);
  const gatewayPackage = JSON.parse(contents.gatewayPackage);
  const gatewayPackageLock = JSON.parse(contents.gatewayPackageLock);
  const cargo = /^version = "([^"]+)"/m.exec(contents.cargo)?.[1];
  const cargoLock = /\[\[package\]\]\r?\nname = "agent-relay"\r?\nversion = "([^"]+)"/.exec(
    contents.cargoLock,
  )?.[1];
  const readme = /Current release: \*\*([^*]+)\*\*/.exec(contents.readme)?.[1];
  const landingPage = /<span class="site-version">v([^<]+)<\/span>/.exec(
    contents.landingPage,
  )?.[1];
  return {
    "package.json": packageJson.version,
    "package-lock.json": packageLock.version,
    "package-lock.json root": packageLock.packages?.[""]?.version,
    "src-tauri/Cargo.toml": cargo,
    "src-tauri/Cargo.lock": cargoLock,
    "src-tauri/tauri.conf.json": tauri.version,
    "channel-gateway/package.json": gatewayPackage.version,
    "channel-gateway/package-lock.json": gatewayPackageLock.version,
    "channel-gateway/package-lock.json root": gatewayPackageLock.packages?.[""]?.version,
    "README.md": readme,
    "index.html": landingPage,
  };
}

function synchronizedVersion(versions) {
  const unique = new Set(Object.values(versions));
  if (unique.has(undefined) || unique.size !== 1) {
    throw new Error(
      `version metadata is not synchronized:\n${Object.entries(versions)
        .map(([file, version]) => `  ${file}: ${version ?? "missing"}`)
        .join("\n")}`,
    );
  }
  return [...unique][0];
}

function nextVersion(current, request, allowMajor) {
  const [major, minor, patch] = parseVersion(current);
  let next;
  if (request === "patch") next = [major, minor, patch + 1];
  else if (request === "minor") next = [major, minor + 1, 0];
  else if (request === "major") next = [major + 1, 0, 0];
  else next = parseVersion(request);

  if (next[0] !== major && !allowMajor) {
    throw new Error("major version changes require --allow-major and explicit user approval");
  }
  if (
    next[0] < major ||
    (next[0] === major && next[1] < minor) ||
    (next[0] === major && next[1] === minor && next[2] <= patch)
  ) {
    throw new Error(`new version must be greater than ${current}`);
  }
  return next.join(".");
}

const contents = Object.fromEntries(
  await Promise.all(
    Object.entries(files).map(async ([key, path]) => [key, await readFile(path, "utf8")]),
  ),
);
const current = synchronizedVersion(extractVersions(contents));
const request = process.argv[2];

if (request === "check") {
  console.log(`Agent Relay version ${current} is synchronized.`);
  process.exit(0);
}
if (!request) {
  throw new Error("usage: npm run version:bump -- <patch|minor|version> [--allow-major]");
}

const next = nextVersion(current, request, process.argv.includes("--allow-major"));
const updated = {
  package: replaceOne(
    contents.package,
    /("version"\s*:\s*")[^"]+("\s*,)/,
    `$1${next}$2`,
    "package.json",
  ),
  packageLock: replaceOne(
    replaceOne(
      contents.packageLock,
      /^(\{\s*"name"\s*:\s*"agent-relay"\s*,\s*"version"\s*:\s*")[^"]+(")/,
      `$1${next}$2`,
      "package-lock.json top-level",
    ),
    /(""\s*:\s*\{\s*"name"\s*:\s*"agent-relay"\s*,\s*"version"\s*:\s*")[^"]+(")/,
    `$1${next}$2`,
    "package-lock.json root package",
  ),
  cargo: replaceOne(
    contents.cargo,
    /(^\[package\]\s*\r?\nname = "agent-relay"\s*\r?\nversion = ")[^"]+(")/m,
    `$1${next}$2`,
    "Cargo.toml",
  ),
  cargoLock: replaceOne(
    contents.cargoLock,
    /(\[\[package\]\]\s*\r?\nname = "agent-relay"\s*\r?\nversion = ")[^"]+(")/,
    `$1${next}$2`,
    "Cargo.lock",
  ),
  tauri: replaceOne(
    contents.tauri,
    /("productName"\s*:\s*"Agent Relay"\s*,\s*"version"\s*:\s*")[^"]+(")/,
    `$1${next}$2`,
    "tauri.conf.json",
  ),
  gatewayPackage: replaceOne(
    contents.gatewayPackage,
    /("version"\s*:\s*")[^"]+("\s*,)/,
    `$1${next}$2`,
    "channel-gateway/package.json",
  ),
  gatewayPackageLock: replaceOne(
    replaceOne(
      contents.gatewayPackageLock,
      /^(\{\s*"name"\s*:\s*"@agent-relay\/channel-gateway"\s*,\s*"version"\s*:\s*")[^"]+("\s*,)/,
      `$1${next}$2`,
      "channel-gateway/package-lock.json top-level",
    ),
    /(""\s*:\s*\{\s*"name"\s*:\s*"@agent-relay\/channel-gateway"\s*,\s*"version"\s*:\s*")[^"]+("\s*,)/,
    `$1${next}$2`,
    "channel-gateway/package-lock.json root package",
  ),
  readme: replaceOne(
    contents.readme,
    /(Current release: \*\*)[^*]+(\*\*)/,
    `$1${next}$2`,
    "README.md",
  ),
  landingPage: replaceOne(
    contents.landingPage,
    /(<span class="site-version">v)[^<]+(<\/span>)/,
    `$1${next}$2`,
    "index.html",
  ),
};

await Promise.all(
  Object.entries(updated).map(([key, value]) => writeFile(files[key], value, "utf8")),
);
console.log(`Bumped Agent Relay ${current} -> ${next}.`);
