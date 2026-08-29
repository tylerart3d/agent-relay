export type ChannelHarness = "direct" | "hermes" | "open_code" | "pi";

export interface ChannelAdapterStatus {
  adapter_id: string;
  channel: string;
  account_id?: string;
  display_name: string;
  state: "connected" | "error";
  online: boolean;
  last_seen_ms: number;
  error?: string;
}

export interface ChannelRoute {
  channel: string;
  account_id: string;
  conversation_id: string;
  session_id: number;
  conversation_label?: string;
  harness: ChannelHarness;
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

export interface ChannelCommandResult {
  ok: boolean;
  handled: boolean;
  command?: string;
  message?: string;
  error?: string;
  confirmation_required?: boolean;
  retry_command?: string;
  route?: ChannelRoute;
  http_status: number;
}

export interface OpenCodeSessionInfo {
  id: string;
  title: string;
  project_id: string;
  project_name: string;
  directory: string;
  updated_at_ms: number;
  archived: boolean;
}

export function channelConversationKey(route: ChannelRoute) {
  return `${route.channel}\u0000${route.account_id}\u0000${route.conversation_id}`;
}

export function channelConversationLabel(route: ChannelRoute) {
  return route.conversation_label?.trim() || route.conversation_id;
}

export function channelHarnessLabel(harness: ChannelHarness) {
  if (harness === "open_code") return "OpenCode";
  if (harness === "pi") return "Pi";
  if (harness === "hermes") return "Hermes";
  return "Direct model";
}
