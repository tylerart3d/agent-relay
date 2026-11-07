#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
  echo "This build script requires an Apple Silicon Mac." >&2
  exit 1
fi

for command_name in npm cargo xcrun; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "Missing required command: $command_name" >&2
    exit 1
  fi
done
xcrun --find clang >/dev/null

sidecar="src-tauri/binaries/llama-swap-aarch64-apple-darwin"
if [ ! -f "$sidecar" ]; then
  echo "Missing bundled llama-swap sidecar: $sidecar" >&2
  exit 1
fi
chmod +x "$sidecar"

if command -v tailscale >/dev/null 2>&1; then
  tailscale_ip=$(tailscale ip -4 | head -n 1)
elif [ -x /Applications/Tailscale.app/Contents/MacOS/Tailscale ]; then
  tailscale_ip=$(TAILSCALE_BE_CLI=1 /Applications/Tailscale.app/Contents/MacOS/Tailscale ip -4 | head -n 1)
else
  echo "Tailscale CLI not found in PATH or the standard app bundle." >&2
  exit 1
fi
if [ -z "$tailscale_ip" ]; then
  echo "Tailscale is installed but did not return an IPv4 address." >&2
  exit 1
fi

echo "Building Agent Relay for arm64 on Tailscale address $tailscale_ip"
npm ci
npm run tauri build -- --bundles app,dmg

app_bundle="src-tauri/target/release/bundle/macos/Agent Relay.app"
if [ ! -d "$app_bundle" ]; then
  echo "Build did not produce the expected app bundle: $app_bundle" >&2
  exit 1
fi
codesign --verify --deep --strict "$app_bundle"

echo "Build complete. Bundles are under src-tauri/target/release/bundle/."
