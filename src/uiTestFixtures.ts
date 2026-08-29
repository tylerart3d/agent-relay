import type { FleetSnapshot, HarnessStatus, HostStatus, ModelProfile } from "./fleet";
import type { AppSettings } from "./settings";

export const testSettings: AppSettings = {
  theme: "dark",
  harness_visibility: {
    opencode: true,
    opencode_cli: true,
    codex: true,
    claude_code: true,
    copilot: true,
    vscode: true,
    pi: true,
    hermes: true,
    hermes_cli: true,
  },
  run_on_startup: false,
  hermes_context_window: 65_536,
  opencode_context_window: 65_536,
  channel_gateway: {
    primary_host_id: "workstation",
    secondary_host_id: "m1-pro",
    automatic_failover: true,
    failover_after_seconds: 60,
    photon_project_id: "project-id",
    allowed_senders: ["+15551234567"],
  },
  photon_credentials_configured: true,
  inference_overrides: {},
};

export function testModel(id: string, displayName = id): ModelProfile {
  return {
    id,
    display_name: displayName,
    runtime: "llama.cpp",
    kind: "text",
    capabilities: ["chat", "completions", "responses", "anthropic_messages"],
    lifecycle_adapter: "llama_swap",
    resource_pool: "default",
    inference_controls: {},
  };
}

export function testHost(
  id: string,
  displayName: string,
  connection: HostStatus["connection"],
  models: ModelProfile[],
  loadedModelId: string | null = null,
): HostStatus {
  return {
    id,
    display_name: displayName,
    address: id,
    hardware: "test hardware",
    connection,
    models,
    loaded_model_id: loadedModelId,
    active_requests: 0,
    memory_used_bytes: null,
    memory_total_bytes: null,
    memory_kind: null,
    tokens_per_second: null,
    aggregate_tokens_per_second: null,
    throughput_concurrency: 0,
    last_seen_at_ms: 1,
    error: null,
    llama_swap: {
      state: "ready",
      version: "test",
      endpoint: "http://127.0.0.1:38474",
      config_path: "llama-swap.yaml",
      pid: 42,
      error: null,
    },
    channel_gateway: null,
  };
}

function harness(selectedModel: string | null = null): HarnessStatus {
  return {
    state: "synced",
    config_path: null,
    selected_model: selectedModel,
    last_synced_at_ms: 1,
    error: null,
  };
}

export function testFleet(): FleetSnapshot {
  const m1Model = testModel("m1-running", "M1 Running");
  const workstationModels = Array.from({ length: 9 }, (_, index) =>
    testModel(`workstation-${index + 1}`, `Workstation Model ${index + 1}`),
  );
  return {
    local_host_id: "m1-pro",
    config_path: "fleet.json",
    proxy_endpoint: "http://127.0.0.1:38475/v1",
    refreshed_at_ms: 1,
    peer_api: { state: "listening", address: "100.0.0.1:38473", error: null },
    hosts: [
      testHost("m1-pro", "M1 Pro", "local", [m1Model], m1Model.id),
      testHost("workstation", "WORKSTATION", "online", workstationModels, workstationModels[0].id),
      testHost("air-m4", "Air-M4", "online", [testModel("air-idle", "Air Idle")]),
    ],
    opencode: { ...harness(), model_count: 11 },
    hermes: { ...harness(), executable_path: null },
    hermes_cli: { ...harness(), executable_path: null },
    codex: harness(),
    claude_code: harness(),
    pi: harness(),
    copilot: harness(),
    vscode: harness(),
  };
}
