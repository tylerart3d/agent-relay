export interface ChannelAddress {
  channel: string;
  account_id: string;
  conversation_id: string;
}

export interface ChannelRoute extends ChannelAddress {
  session_id: number;
  conversation_label?: string;
  harness: "direct" | "hermes" | "open_code" | "pi";
  harness_host_id?: string;
  host_id: string;
  model_id: string;
  project?: string;
  archived_at_ms?: number;
  native_session_id?: string;
  handoff_from_session_id?: number;
  handoff_status?: "pending" | "completed";
  handoff_completed_at_ms?: number;
  native_archive_status?: "pending" | "completed" | "failed";
  native_archive_error?: string;
  native_archived_at_ms?: number;
  updated_at_ms: number;
}

export interface ChannelCommandRequest extends ChannelAddress {
  sender_id: string;
  conversation_label?: string;
  external_message_id?: string;
  text: string;
}

export interface ChannelCommandResponse {
  ok: boolean;
  handled: boolean;
  command?: string;
  message?: string;
  mobile_message?: string;
  error?: string;
  confirmation_required?: boolean;
  retry_command?: string;
  route?: ChannelRoute;
  context_handoff?: "not_requested" | "pending_first_destination_reply";
  native_harness_archive?:
    | "not_requested"
    | "pending_first_destination_reply"
    | "completed"
    | "failed_retry_pending";
  hosts?: unknown[];
  models?: unknown[];
  sessions?: ChannelRoute[];
}

export interface ChannelDeliveryResponse {
  ok: boolean;
  handled: true;
  reply: string;
  route: ChannelRoute;
  session_mode: "stateless" | "native";
  finish_reason?: string;
  usage?: unknown;
  replayed?: boolean;
}

export interface ChannelAdapterHeartbeat {
  adapter_id: string;
  channel: string;
  account_id?: string;
  display_name: string;
  state: "connected" | "error";
  error?: string;
}

export interface ChannelAdapterHeartbeatResponse {
  ok: boolean;
  error?: string;
}

export interface GatewayDecision {
  mode: "active" | "standby" | "disabled";
  host_id: string;
  reason: string;
  retry_after_ms: number;
}

export class AgentRelayClient {
  constructor(
    private readonly endpoint: string,
    private readonly fetchImpl: typeof fetch = fetch,
  ) {}

  async command(request: ChannelCommandRequest): Promise<ChannelCommandResponse> {
    return this.post<ChannelCommandResponse>("/api/v1/channels/command", request);
  }

  async deliver(request: ChannelCommandRequest): Promise<ChannelDeliveryResponse> {
    return this.post<ChannelDeliveryResponse>("/api/v1/channels/deliver", request);
  }

  async heartbeat(request: ChannelAdapterHeartbeat): Promise<void> {
    await this.post<ChannelAdapterHeartbeatResponse>("/api/v1/channels/adapters/heartbeat", request);
  }

  async gatewayHeartbeat(state: "active" | "standby" | "error", error?: string): Promise<void> {
    await this.post<ChannelAdapterHeartbeatResponse>("/api/v1/channels/gateway/heartbeat", {
      state,
      error,
    });
  }

  async gatewayDecision(): Promise<GatewayDecision> {
    const response = await this.fetchImpl(`${this.endpoint}/api/v1/channels/gateway/decision`, {
      signal: AbortSignal.timeout(10_000),
    });
    const result = (await response.json()) as {
      ok: boolean;
      error?: string;
      decision?: GatewayDecision;
    };
    if (!response.ok || !result.ok || !result.decision) {
      throw new Error(result.error ?? `Agent Relay returned HTTP ${response.status}`);
    }
    return result.decision;
  }

  private async post<T extends { ok: boolean; error?: string }>(path: string, body: object): Promise<T> {
    const response = await this.fetchImpl(`${this.endpoint}${path}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(27 * 60_000),
    });
    const result = (await response.json()) as T;
    if (!response.ok || !result.ok) {
      throw new Error(result.error ?? `Agent Relay returned HTTP ${response.status}`);
    }
    return result;
  }
}
