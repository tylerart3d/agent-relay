import { mkdtemp } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { describe, expect, it, vi } from "vitest";
import { MessageCheckpointStore } from "./dedupe.js";
import { processInboundMessage } from "./processor.js";

function input(text = "!ar status") {
  return {
    channel: "imessage",
    account_id: "+15550000000",
    conversation_id: "chat-1",
    sender_id: "+15551234567",
    message_id: "message-1",
    text,
  };
}

describe("processInboundMessage", () => {
  it("ignores senders outside the allowlist", async () => {
    const checkpoints = new MessageCheckpointStore(join(await mkdtemp(join(tmpdir(), "ar-")), "ids.json"));
    const client = { command: vi.fn(), deliver: vi.fn() } as never;
    expect(await processInboundMessage(input(), new Set(["+15557654321"]), checkpoints, client)).toEqual({ kind: "ignored" });
  });

  it("replies to commands and checkpoints only after success", async () => {
    const checkpoints = new MessageCheckpointStore(join(await mkdtemp(join(tmpdir(), "ar-")), "ids.json"));
    const command = vi.fn().mockResolvedValue({ ok: true, handled: true, command: "status", message: "Ready" });
    const client = { command, deliver: vi.fn() } as never;
    const result = await processInboundMessage(input(), new Set(["+15551234567"]), checkpoints, client);
    expect(result).toMatchObject({ kind: "command", reply: "Ready" });
    expect(command).toHaveBeenCalledWith(expect.objectContaining({ external_message_id: "message-1" }));
    expect(await processInboundMessage(input(), new Set(["+15551234567"]), checkpoints, client)).toEqual({
      kind: "duplicate",
      reply: "Ready",
    });
    await checkpoints.markDelivered("message-1");
    expect(await processInboundMessage(input(), new Set(["+15551234567"]), checkpoints, client)).toEqual({
      kind: "duplicate",
      reply: undefined,
    });
    expect(command).toHaveBeenCalledTimes(1);
  });

  it("delivers ordinary messages through Agent Relay and returns its reply", async () => {
    const checkpoints = new MessageCheckpointStore(join(await mkdtemp(join(tmpdir(), "ar-")), "ids.json"));
    const route = { host_id: "workstation", model_id: "qwen", harness: "hermes" };
    const deliver = vi.fn().mockResolvedValue({ ok: true, handled: true, route, reply: "hello back", session_mode: "stateless" });
    const client = { command: vi.fn().mockResolvedValue({ ok: true, handled: false, route }), deliver } as never;
    const result = await processInboundMessage(input("hello"), new Set(["+15551234567"]), checkpoints, client);
    expect(result).toMatchObject({ kind: "message", response: { route }, reply: "hello back" });
    expect(deliver).toHaveBeenCalledWith(expect.objectContaining({ external_message_id: "message-1" }));
  });

  it("does not checkpoint a failed Agent Relay request", async () => {
    const checkpoints = new MessageCheckpointStore(join(await mkdtemp(join(tmpdir(), "ar-")), "ids.json"));
    const client = { command: vi.fn().mockRejectedValue(new Error("offline")), deliver: vi.fn() } as never;
    await expect(processInboundMessage(input(), new Set(["+15551234567"]), checkpoints, client)).rejects.toThrow("offline");
    expect(await checkpoints.has("message-1")).toBe(false);
  });
});
