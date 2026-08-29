import { Spectrum } from "spectrum-ts";
import { imessage } from "spectrum-ts/providers/imessage";
import { AgentRelayClient } from "./agentRelayClient.js";
import { loadConfig } from "./config.js";
import { ConversationQueue } from "./conversationQueue.js";
import { MessageCheckpointStore } from "./dedupe.js";
import { processInboundMessage } from "./processor.js";

const config = loadConfig();
const checkpoints = new MessageCheckpointStore(config.checkpointPath);
const client = new AgentRelayClient(config.agentRelayEndpoint);
const conversations = new ConversationQueue();

const waitForGatewayOwnership = async () => {
  for (;;) {
    try {
      const decision = await client.gatewayDecision();
      if (decision.mode === "active") {
        await client.gatewayHeartbeat("active");
        return;
      }
      await client.gatewayHeartbeat("standby");
      await new Promise((resolve) => setTimeout(resolve, Math.max(1_000, decision.retry_after_ms)));
    } catch (error) {
      console.error(
        "Agent Relay gateway election is unavailable",
        error instanceof Error ? error.message : error,
      );
      await new Promise((resolve) => setTimeout(resolve, 5_000));
    }
  }
};

await waitForGatewayOwnership();
const app = await Spectrum({
  projectId: config.projectId,
  projectSecret: config.projectSecret,
  providers: [imessage.config()],
  telemetry: false,
});
let activeAccountId: string | undefined;
const heartbeat = async () => {
  try {
    await client.gatewayHeartbeat("active");
    await client.heartbeat({
      adapter_id: config.adapterId,
      channel: "imessage",
      account_id: activeAccountId,
      display_name: "Photon iMessage",
      state: "connected",
    });
  } catch (error) {
    console.error("Agent Relay could not publish the Photon heartbeat", error);
  }
};
await heartbeat();
const heartbeatTimer = setInterval(() => void heartbeat(), 10_000);

for await (const [space, message] of app.messages) {
  if (message.direction !== "inbound" || message.content.type !== "text" || !message.sender) continue;
  const accountId = "phone" in space && typeof space.phone === "string" ? space.phone : undefined;
  if (!accountId) {
    console.error("Agent Relay ignored an iMessage without a receiving account id");
    continue;
  }
  activeAccountId = accountId;
  void heartbeat();

  void conversations.enqueue(space.id, async () => {
    let reply: string | undefined;
    try {
      await space.responding(async () => {
        try {
          const result = await processInboundMessage(
            {
              channel: "imessage",
              account_id: accountId,
              conversation_id: space.id,
              sender_id: message.sender!.id,
              message_id: message.id,
              text: message.content.type === "text" ? message.content.text : "",
            },
            config.allowedSenders,
            checkpoints,
            client,
          );
          reply = result.reply;
        } catch (error) {
          console.error("Agent Relay could not process an inbound iMessage", error instanceof Error ? error.message : error);
          reply = `Agent Relay couldn't deliver that message: ${error instanceof Error ? error.message : "unknown error"}`;
          try {
            await checkpoints.recordReply(message.id, reply);
          } catch (checkpointError) {
            console.error("Agent Relay could not save the delivery error", checkpointError);
            return;
          }
        }

        if (!reply) return;
        try {
          await message.reply(reply);
          await checkpoints.markDelivered(message.id);
        } catch (replyError) {
          console.error(
            "Agent Relay could not send the saved iMessage reply; it will retry if Photon redelivers the event",
            replyError instanceof Error ? replyError.message : replyError,
          );
        }
      });
    } catch (error) {
      console.error("Agent Relay iMessage conversation worker failed", error);
    }
  });
}

clearInterval(heartbeatTimer);
await conversations.drain();
