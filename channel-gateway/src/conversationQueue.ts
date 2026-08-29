export class ConversationQueue {
  private readonly tails = new Map<string, Promise<void>>();

  enqueue(key: string, task: () => Promise<void>): Promise<void> {
    const previous = this.tails.get(key) ?? Promise.resolve();
    const run = previous.catch(() => undefined).then(task);
    const tracked = run.finally(() => {
      if (this.tails.get(key) === tracked) this.tails.delete(key);
    });
    this.tails.set(key, tracked);
    return run;
  }

  async drain(): Promise<void> {
    await Promise.allSettled([...this.tails.values()]);
  }

  get activeConversations(): number {
    return this.tails.size;
  }
}
