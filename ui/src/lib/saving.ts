// One editor's saves, run one at a time so each sends the state the last
// one left. Shared by the widget editor and the group editor.

import type { CommandError } from "../api";

/** One input's draft and messages. */
export interface Slot {
  /** The current draft, or undefined when there is none. */
  draft(): unknown;
  clearDraft(): void;
  setError(message: string | null): void;
  setSaved(on: boolean): void;
}

/** `onchanged` refetches the state and resolves once the new state is in. */
export function createSaver(onchanged: () => Promise<void>) {
  let queue: Promise<void> = Promise.resolve();
  const savedTimers = new Map<string, ReturnType<typeof setTimeout>>();

  /** Runs `job` once every job queued before it has finished. */
  function run(job: () => Promise<void>): Promise<void> {
    const mine = queue.then(job, job);
    queue = mine.catch(() => {});
    return mine;
  }

  function flashSaved(name: string, slot: Slot) {
    slot.setSaved(true);
    clearTimeout(savedTimers.get(name));
    savedTimers.set(
      name,
      setTimeout(() => slot.setSaved(false), 2000),
    );
  }

  /**
   * Saves one input: waits its turn, skips a draft edited again meanwhile
   * (that edit saves on its own commit), sends, and waits for the refetched
   * state before dropping the draft. `send` reads the state only when it
   * runs, so it builds on everything saved before it.
   */
  function save(
    name: string,
    slot: Slot,
    value: unknown,
    send: () => Promise<void>,
    failed: (e: CommandError) => void,
  ): Promise<void> {
    return run(async () => {
      if (slot.draft() !== value) return;
      try {
        await send();
        slot.setError(null);
        // Resolves once state at least this new is in (latestRefresher).
        await onchanged();
        if (slot.draft() === value) slot.clearDraft();
        flashSaved(name, slot);
      } catch (e) {
        failed(e as CommandError);
      }
    });
  }

  return { run, save };
}
