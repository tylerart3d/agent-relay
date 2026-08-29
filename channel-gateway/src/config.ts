import { resolve } from "node:path";

export interface GatewayConfig {
  projectId: string;
  projectSecret: string;
  agentRelayEndpoint: string;
  allowedSenders: Set<string>;
  checkpointPath: string;
  adapterId: string;
}

function required(env: NodeJS.ProcessEnv, name: string): string {
  const value = env[name]?.trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}

export function normalizeSender(value: string): string {
  return value.trim().toLowerCase().replace(/[\s()-]/g, "");
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): GatewayConfig {
  const allowedSenders = new Set(
    required(env, "AGENT_RELAY_ALLOWED_SENDERS")
      .split(",")
      .map(normalizeSender)
      .filter(Boolean),
  );

  return {
    projectId: required(env, "PHOTON_PROJECT_ID"),
    projectSecret: required(env, "PHOTON_PROJECT_SECRET"),
    agentRelayEndpoint: (env.AGENT_RELAY_ENDPOINT ?? "http://127.0.0.1:38475").replace(/\/$/, ""),
    allowedSenders,
    checkpointPath: resolve(env.AGENT_RELAY_CHECKPOINT_PATH ?? ".agent-relay-channel-checkpoints.json"),
    adapterId: env.AGENT_RELAY_ADAPTER_ID?.trim() || "photon-imessage",
  };
}
