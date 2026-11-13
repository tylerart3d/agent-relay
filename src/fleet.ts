export type ConnectionState = "local" | "online" | "offline";
export type LlamaSwapState = "unknown" | "starting" | "ready" | "stopped" | "error";

export interface LlamaSwapStatus {
  state: LlamaSwapState;
  version: string;
  endpoint: string;
  config_path: string;
  pid: number | null;
  error: string | null;
}

export interface ModelProfile {
  priority?: "foreground" | "background";
  id: string;
  display_name: string;
  runtime: string;
  kind: "text" | "image" | "worker" | "voice" | "unknown";
  capabilities: Array<
    | "chat"
    | "completions"
    | "responses"
    | "anthropic_messages"
    | "embeddings"
    | "vision_input"
    | "image_generation"
    | "workflow_queue"
    | "http_service"
    | "realtime_audio"
    | "text_to_speech"
    | "speech_to_text"
    | "unknown"
  >;
  lifecycle_adapter: string;
  resource_pool: string;
  context_length?: number | null;
  parallel_slots?: number | null;
  inference_controls: {
    thinking?: {
      adapter: string;
      efforts: ReasoningEffort[];
      default_effort?: ReasoningEffort | null;
      budget_min?: number | null;
      budget_max?: number | null;
      budget_step?: number | null;
      default_budget?: number | null;
      effort_budgets?: Partial<Record<ReasoningEffort, number>>;
    } | null;
    /** Set by the model profile; not adjustable per client. */
    temperature?: { default?: number | null } | null;
  };
}

export type ReasoningEffort = "off" | "on" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";

export interface InferenceOverrides {
  reasoning_effort?: ReasoningEffort | null;
}

export interface HostStatus {
  reclaim?: { models: string[]; target: string | null; active_requests: number; reserved: boolean };
  gpu_activity?: { busy: boolean; external_percent: number | null; sampled_at_ms: number } | null;
  available?: boolean;
  routing_version?: number;
  id: string;
  display_name: string;
  address: string;
  hardware: string;
  connection: ConnectionState;
  models: ModelProfile[];
  loaded_model_id: string | null;
  active_requests: number;
  memory_used_bytes: number | null;
  memory_total_bytes: number | null;
  memory_kind: string | null;
  tokens_per_second: number | null;
  aggregate_tokens_per_second: number | null;
  throughput_concurrency: number;
  last_seen_at_ms: number | null;
  error: string | null;
  llama_swap: LlamaSwapStatus;
}


export interface FleetSnapshot {
  local_host_id: string;
  config_path: string;
  proxy_endpoint: string;
  refreshed_at_ms: number;
  peer_api: PeerApiStatus;
  hosts: HostStatus[];
  opencode: OpenCodeStatus;
  hermes: HermesStatus;
  hermes_cli: HermesStatus;
  codex: HarnessStatus;
  openbot: HarnessStatus;
  claude_code: HarnessStatus;
  pi: HarnessStatus;
  copilot: HarnessStatus;
  vscode: HarnessStatus;
}

export interface HarnessStatus {
  state: "disabled" | "pending" | "synced" | "error";
  config_path: string | null;
  selected_model: string | null;
  last_synced_at_ms: number | null;
  error: string | null;
}

export interface PeerApiStatus {
  state: "starting" | "listening" | "error";
  address: string | null;
  error: string | null;
}

export interface OpenCodeStatus {
    state: "disabled" | "pending" | "synced" | "error";
    config_path: string | null;
    model_count: number;
    selected_model: string | null;
    last_synced_at_ms: number | null;
  error: string | null;
}

export interface HermesStatus {
  state: "disabled" | "pending" | "synced" | "error";
  executable_path: string | null;
  selected_model: string | null;
  last_synced_at_ms: number | null;
  error: string | null;
}

export interface ControlOutcome {
  state: "applied" | "conflict" | "noop";
  host_id: string;
  active_requests: number;
  loaded_model_id: string | null;
  message: string;
}

export interface ModelTelemetrySummary {
  host_id: string;
  model_id: string;
  request_count: number;
  output_tokens: number;
  average_tokens_per_second: number | null;
  average_ttft_ms: number | null;
  failed_requests: number;
}

export interface LifecycleEventSummary {
  occurred_at_ms: number;
  host_id: string;
  model_id: string | null;
  action: string;
  outcome: string;
  duration_ms: number;
  forced: boolean;
}

export interface TelemetrySummary {
  range_hours: number;
  generated_at_ms: number;
  request_count: number;
  successful_requests: number;
  failed_requests: number;
  prompt_tokens: number;
  output_tokens: number;
  average_tokens_per_second: number | null;
  average_ttft_ms: number | null;
  models: ModelTelemetrySummary[];
  recent_lifecycle: LifecycleEventSummary[];
}
