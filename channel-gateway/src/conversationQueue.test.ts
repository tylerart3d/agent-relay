import { describe, expect, it } from "vitest";
import { ConversationQueue } from "./conversationQueue.js";

describe("ConversationQueue", () => {
  it("keeps messages ordered within one conversation", async () => {
    const queue = new ConversationQueue();
    const events: string[] = [];
    let releaseFirst: (() => void) | undefined;
    let signalStarted: (() => void) | undefined;
    const gate = new Promise<void>((resolve) => { releaseFirst = resolve; });
    const started = new Promise<void>((resolve) => { signalStarted = resolve; });

    const first = queue.enqueue("chat-1", async () => {
      events.push("first-start");
      signalStarted?.();
      await gate;
      events.push("first-end");
    });
    const second = queue.enqueue("chat-1", async () => {
      events.push("second");
    });

    await started;
    expect(events).toEqual(["first-start"]);
    releaseFirst?.();
    await Promise.all([first, second]);
    expect(events).toEqual(["first-start", "first-end", "second"]);
  });

  it("allows independent conversations to run concurrently", async () => {
    const queue = new ConversationQueue();
    const events: string[] = [];
    let releaseSlow: (() => void) | undefined;
    const gate = new Promise<void>((resolve) => { releaseSlow = resolve; });

    const slow = queue.enqueue("slow", async () => {
      events.push("slow-start");
      await gate;
    });
    const fast = queue.enqueue("fast", async () => {
      events.push("fast");
    });

    await fast;
    expect(events).toEqual(["slow-start", "fast"]);
    expect(queue.activeConversations).toBe(1);
    releaseSlow?.();
    await slow;
    await queue.drain();
    expect(queue.activeConversations).toBe(0);
  });

  it("continues a conversation after a failed message", async () => {
    const queue = new ConversationQueue();
    const failed = queue.enqueue("chat", async () => {
      throw new Error("failed");
    });
    const recovered = queue.enqueue("chat", async () => undefined);
    await expect(failed).rejects.toThrow("failed");
    await expect(recovered).resolves.toBeUndefined();
  });
});
