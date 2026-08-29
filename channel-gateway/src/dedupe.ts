import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { dirname } from "node:path";

export interface MessageCheckpoint {
  messageId: string;
  reply?: string;
  delivered: boolean;
}

interface CheckpointFileV1 {
  version: 1;
  messageIds: string[];
}

interface CheckpointFileV2 {
  version: 2;
  messages: MessageCheckpoint[];
}

export class MessageCheckpointStore {
  private readonly messages = new Map<string, MessageCheckpoint>();
  private loaded = false;
  private operationTail: Promise<void> = Promise.resolve();

  constructor(
    private readonly path: string,
    private readonly limit = 4096,
  ) {}

  get(messageId: string): Promise<MessageCheckpoint | undefined> {
    return this.runExclusive(async () => {
      await this.load();
      const checkpoint = this.messages.get(messageId);
      return checkpoint ? { ...checkpoint } : undefined;
    });
  }

  async has(messageId: string): Promise<boolean> {
    return (await this.get(messageId)) !== undefined;
  }

  recordReply(messageId: string, reply: string): Promise<void> {
    return this.runExclusive(async () => {
      await this.load();
      this.upsert({ messageId, reply, delivered: false });
      await this.persist();
    });
  }

  markDelivered(messageId: string): Promise<void> {
    return this.runExclusive(async () => {
      await this.load();
      const checkpoint = this.messages.get(messageId);
      if (!checkpoint) throw new Error(`cannot deliver unknown message ${messageId}`);
      this.upsert({ ...checkpoint, delivered: true });
      await this.persist();
    });
  }

  private runExclusive<T>(operation: () => Promise<T>): Promise<T> {
    const run = this.operationTail.catch(() => undefined).then(operation);
    this.operationTail = run.then(
      () => undefined,
      () => undefined,
    );
    return run;
  }

  private upsert(checkpoint: MessageCheckpoint): void {
    this.messages.delete(checkpoint.messageId);
    this.messages.set(checkpoint.messageId, checkpoint);
    while (this.messages.size > this.limit) {
      const oldest = this.messages.keys().next().value;
      if (oldest === undefined) break;
      this.messages.delete(oldest);
    }
  }

  private async load(): Promise<void> {
    if (this.loaded) return;
    this.loaded = true;
    try {
      const parsed = JSON.parse(await readFile(this.path, "utf8")) as
        | Partial<CheckpointFileV1>
        | Partial<CheckpointFileV2>;
      if (parsed.version === 1 && "messageIds" in parsed && Array.isArray(parsed.messageIds)) {
        for (const messageId of parsed.messageIds.slice(-this.limit)) {
          if (typeof messageId === "string" && messageId) {
            this.upsert({ messageId, delivered: true });
          }
        }
        return;
      }
      if (parsed.version !== 2 || !("messages" in parsed) || !Array.isArray(parsed.messages)) return;
      for (const checkpoint of parsed.messages.slice(-this.limit)) {
        if (
          checkpoint &&
          typeof checkpoint.messageId === "string" &&
          checkpoint.messageId &&
          typeof checkpoint.delivered === "boolean" &&
          (checkpoint.reply === undefined || typeof checkpoint.reply === "string")
        ) {
          this.upsert({ ...checkpoint });
        }
      }
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    }
  }

  private async persist(): Promise<void> {
    await mkdir(dirname(this.path), { recursive: true });
    const temporaryPath = `${this.path}.${process.pid}.tmp`;
    const payload: CheckpointFileV2 = { version: 2, messages: [...this.messages.values()] };
    await writeFile(temporaryPath, `${JSON.stringify(payload)}\n`, { encoding: "utf8", mode: 0o600 });
    await rename(temporaryPath, this.path);
  }
}
