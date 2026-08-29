import type {
  AgentRelayClient,
  ChannelCommandRequest,
  ChannelCommandResponse,
  ChannelDeliveryResponse,
} from "./agentRelayClient.js";
import type { MessageCheckpointStore } from "./dedupe.js";
import { formatCommandResponse } from "./format.js";
import { normalizeSender } from "./config.js";

export interface NormalizedInboundMessage extends ChannelCommandRequest {
  message_id: string;
}

export interface ProcessResult {
  kind: "ignored" | "duplicate" | "command" | "message";
  response?: ChannelCommandResponse;
  delivery?: ChannelDeliveryResponse;
  reply?: string;
}

export async function processInboundMessage(
  input: NormalizedInboundMessage,
  allowedSenders: ReadonlySet<string>,
  checkpoints: MessageCheckpointStore,
  client: AgentRelayClient,
): Promise<ProcessResult> {
  if (!allowedSenders.has(normalizeSender(input.sender_id))) return { kind: "ignored" };
  const checkpoint = await checkpoints.get(input.message_id);
  if (checkpoint) {
    return {
      kind: "duplicate",
      reply: checkpoint.delivered ? undefined : checkpoint.reply,
    };
  }

  const { message_id: _messageId, ...baseRequest } = input;
  const request = { ...baseRequest, external_message_id: input.message_id };
  const response = await client.command(request);
  if (response.handled) {
    const reply = formatCommandResponse(response);
    await checkpoints.recordReply(input.message_id, reply);
    return { kind: "command", response, reply };
  }
  const delivery = await client.deliver(request);
  await checkpoints.recordReply(input.message_id, delivery.reply);
  return { kind: "message", response, delivery, reply: delivery.reply };
}
