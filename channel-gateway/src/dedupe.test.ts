import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { describe, expect, it } from "vitest";
import { MessageCheckpointStore } from "./dedupe.js";

describe("MessageCheckpointStore", () => {
  it("persists pending replies and retains only the newest bounded entries", async () => {
    const directory = await mkdtemp(join(tmpdir(), "agent-relay-checkpoints-"));
    const path = join(directory, "messages.json");
    const store = new MessageCheckpointStore(path, 2);
    await store.recordReply("one", "reply one");
    await store.recordReply("two", "reply two");
    await store.markDelivered("two");
    await store.recordReply("three", "reply three");

    const reloaded = new MessageCheckpointStore(path, 2);
    expect(await reloaded.has("one")).toBe(false);
    expect(await reloaded.get("two")).toEqual({
      messageId: "two",
      reply: "reply two",
      delivered: true,
    });
    expect(await reloaded.get("three")).toEqual({
      messageId: "three",
      reply: "reply three",
      delivered: false,
    });
    expect(JSON.parse(await readFile(path, "utf8"))).toEqual({
      version: 2,
      messages: [
        { messageId: "two", reply: "reply two", delivered: true },
        { messageId: "three", reply: "reply three", delivered: false },
      ],
    });
  });

  it("migrates delivered message ids from the original checkpoint format", async () => {
    const directory = await mkdtemp(join(tmpdir(), "ar-legacy-"));
    const path = join(directory, "messages.json");
    await writeFile(path, JSON.stringify({ version: 1, messageIds: ["legacy"] }));
    const store = new MessageCheckpointStore(path);
    expect(await store.get("legacy")).toEqual({ messageId: "legacy", delivered: true });
  });

  it("serializes checkpoint writes from concurrent conversations", async () => {
    const directory = await mkdtemp(join(tmpdir(), "ar-concurrent-checkpoints-"));
    const path = join(directory, "messages.json");
    const store = new MessageCheckpointStore(path);

    await Promise.all(
      Array.from({ length: 32 }, (_, index) =>
        store.recordReply(`message-${index}`, `reply-${index}`),
      ),
    );
    await Promise.all(
      Array.from({ length: 32 }, (_, index) => store.markDelivered(`message-${index}`)),
    );

    const reloaded = new MessageCheckpointStore(path);
    const checkpoints = await Promise.all(
      Array.from({ length: 32 }, (_, index) => reloaded.get(`message-${index}`)),
    );
    expect(checkpoints).toHaveLength(32);
    expect(checkpoints.every((checkpoint) => checkpoint?.delivered)).toBe(true);
  });
});
