import type { ChannelCommandResponse, ChannelRoute } from "./agentRelayClient.js";

function routeLabel(route: ChannelRoute): string {
  const harnessHost = route.harness_host_id ? ` on ${route.harness_host_id}` : "";
  const project = route.project ? ` · ${route.project}` : "";
  const handoff = route.handoff_status === "pending"
    ? " · context transfer pending"
    : route.handoff_status === "completed"
      ? " · context transferred"
      : "";
  return `#${route.session_id} ${route.harness}${harnessHost} → ${route.host_id}/${route.model_id}${project}${handoff}`;
}

export function formatCommandResponse(response: ChannelCommandResponse): string {
  if (response.mobile_message) return response.mobile_message;
  if (response.confirmation_required && response.retry_command) {
    return `${response.message ?? "Confirmation required"}\nReply with: ${response.retry_command}`;
  }
  if (response.command === "sessions" && response.sessions) {
    if (response.sessions.length === 0) return "No Agent Relay sessions for this conversation.";
    return response.sessions.map(routeLabel).join("\n");
  }
  if (response.route) {
    return `${response.message ?? "Route updated"}\n${routeLabel(response.route)}`;
  }
  return response.message ?? "Agent Relay command completed.";
}
