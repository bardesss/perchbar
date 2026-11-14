<script lang="ts">
  // One rotation group: how often it turns, the order its widgets take
  // turns in, and taking widgets out. Every change goes through the group's
  // queue, which the list's "add to group" shares, and reads the stored
  // group only when its turn comes. The parent re-creates this per group.
  import { tick } from "svelte";
  import { ungroup, updateGroup, type CommandError, type Group } from "../api";
  import { checkInterval, GROUP_INTERVAL_MAX, GROUP_INTERVAL_MIN } from "./form";
  import { moveBefore, moveBy } from "./order";
  import type { Saver, Slot } from "./saving";

  interface Props {
    /** The group as shown. */
    group: Group;
    /** The group as stored now, or undefined once it is gone. */
    current: () => Group | undefined;
    /** This group's queue. */
    saver: Saver;
    labels: Record<string, string>;
    /** Shows an instance in the main pane. */
    onselect: (id: string) => void;
    /** Puts focus on a top-level row of the list. */
    focusRow: (id: string) => Promise<void>;
    /** Refetches the state; resolves once the new state is in. */
    onchanged: () => Promise<void>;
  }

  let { group, current, saver, labels, onselect, focusRow, onchanged }: Props = $props();

  /** The drop zone after the last member. Instance ids are never empty. */
  const END = "";

  let interval: { draft: string | null; error: string | null; saved: boolean } = $state({
    draft: null,
    error: null,
    saved: false,
  });
  let notice: string | null = $state(null);
  /** Member order shown until the refetch after a move lands. */
  let pendingMembers: string[] | null = $state(null);
  let moves = 0;
  let dragging: string | null = $state(null);
  let dropTarget: string | null = $state(null);
  let root: HTMLElement | undefined = $state();

  const members = $derived(pendingMembers ?? group.members);

  const intervalSlot: Slot = {
    draft: () => interval.draft ?? undefined,
    clearDraft: () => (interval.draft = null),
    setError: (m) => (interval.error = m),
    setSaved: (on) => (interval.saved = on),
  };

  const gone: CommandError = { message: "This group is gone" };

  function name(id: string): string {
    return labels[id] ?? id;
  }

  function editInterval(raw: string) {
    interval.draft = raw;
    interval.saved = false;
    interval.error = checkInterval(raw);
  }

  function commitInterval() {
    if (interval.draft === null) return;
    const raw = interval.draft;
    const problem = checkInterval(raw);
    if (problem) {
      interval.error = problem;
      return;
    }
    void saver.save(
      "interval",
      intervalSlot,
      raw,
      async () => {
        const stored = current();
        if (!stored) throw gone;
        await updateGroup(stored.id, stored.members, Number(raw.trim()));
      },
      (err) => (interval.error = err.message),
    );
  }

  /**
   * Reorders the members. `compute` runs on the members shown now (for the
   * preview) and again on the stored ones when the change gets its turn.
   */
  function reorder(compute: (members: string[]) => string[] | null): Promise<void> {
    const preview = compute(members);
    if (!preview) return Promise.resolve();
    const mine = ++moves;
    pendingMembers = preview;
    notice = null;
    return saver.run(async () => {
      try {
        const stored = current();
        const next = stored && compute(stored.members);
        if (stored && next) {
          await updateGroup(stored.id, next, stored.interval);
          await onchanged();
        }
      } catch (e) {
        notice = (e as CommandError).message;
      } finally {
        if (mine === moves) pendingMembers = null;
      }
    });
  }

  async function step(id: string, delta: -1 | 1, button: HTMLElement) {
    const which = button.dataset.move;
    await reorder((m) => moveBy(m, id, delta));
    await tick();
    // The row moved; keep focus on the same button of the same row.
    const row = root?.querySelector<HTMLElement>(`li[data-member="${CSS.escape(id)}"]`);
    const again = row?.querySelector<HTMLButtonElement>(`[data-move="${which}"]`);
    // At the end of the list that button is off: take the other one.
    const fallback = row?.querySelector<HTMLButtonElement>("[data-move]:not(:disabled), [data-remove]");
    if (again && !again.disabled) again.focus();
    else fallback?.focus();
  }

  /** The group is gone: show `id` and put focus on its row in the list. */
  async function leave(id: string) {
    onselect(id);
    await focusRow(id);
  }

  function removeMember(id: string) {
    notice = null;
    void saver.run(async () => {
      const stored = current();
      if (!stored || !stored.members.includes(id)) return;
      const index = stored.members.indexOf(id);
      const rest = stored.members.filter((m) => m !== id);
      try {
        // With one widget left the core dissolves the group: that widget
        // takes its place and the removed one follows it.
        await updateGroup(stored.id, rest, stored.interval);
        await onchanged();
        if (rest.length < 2) {
          await leave(id);
          return;
        }
        await tick();
        const next = rest[Math.min(index, rest.length - 1)];
        root
          ?.querySelector<HTMLButtonElement>(`li[data-member="${CSS.escape(next)}"] [data-remove]`)
          ?.focus();
      } catch (e) {
        notice = (e as CommandError).message;
      }
    });
  }

  function dissolve() {
    notice = null;
    void saver.run(async () => {
      const stored = current();
      if (!stored) return;
      try {
        await ungroup(stored.id);
        await onchanged();
        if (stored.members[0]) await leave(stored.members[0]);
      } catch (e) {
        notice = (e as CommandError).message;
      }
    });
  }

  function ondragstart(e: DragEvent, id: string) {
    dragging = id;
    e.dataTransfer?.setData("text/plain", id);
    if (e.dataTransfer) e.dataTransfer.effectAllowed = "move";
  }

  function ondragover(e: DragEvent, target: string) {
    if (!dragging) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
    dropTarget = target;
  }

  function ondrop(e: DragEvent, target: string) {
    if (!dragging) return;
    e.preventDefault();
    const from = dragging;
    dragging = null;
    dropTarget = null;
    // Lands before the row it was dropped on, or last after the last row.
    void reorder((m) => moveBefore(m, from, target === END ? null : target));
  }

  function ondragend() {
    dragging = null;
    dropTarget = null;
  }

  const summary = $derived(
    members.length === 1
      ? "1 widget is in this group."
      : `${members.length} widgets share one spot on the taskbar and take turns.`,
  );
</script>

<article class="editor" aria-labelledby="group-title" bind:this={root}>
  <header>
    <h2 id="group-title">Rotation group</h2>
    <p class="muted">{summary}</p>
  </header>

  {#if notice}<p class="notice field-error" role="alert">{notice}</p>{/if}

  <section class="card form" aria-label="Rotation">
    <div class="field">
      <label for="group-interval">Rotate every</label>
      <span id="group-interval-hint" class="hint muted">
        Seconds, {GROUP_INTERVAL_MIN} to {GROUP_INTERVAL_MAX}.
      </span>
      <input
        id="group-interval"
        class="input interval"
        type="text"
        inputmode="numeric"
        autocomplete="off"
        value={interval.draft ?? String(group.interval)}
        aria-invalid={interval.error ? "true" : undefined}
        aria-describedby="group-interval-hint group-interval-msg"
        oninput={(e) => editInterval((e.currentTarget as HTMLInputElement).value)}
        onblur={commitInterval}
        onkeydown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            commitInterval();
          }
        }}
      />
      <span id="group-interval-msg" class="msg" aria-live="polite">
        {#if interval.error}
          <span class="field-error">{interval.error}</span>
        {:else if interval.saved}
          <span class="field-saved">Saved</span>
        {/if}
      </span>
    </div>
  </section>

  <section class="card" aria-labelledby="group-order">
    <h3 id="group-order">Order</h3>
    <ol class="members" aria-labelledby="group-order">
      {#each members as id, index (id)}
        <li
          data-member={id}
          draggable="true"
          class:dragging={dragging === id}
          class:drop={dropTarget === id && dragging !== id}
          ondragstart={(e) => ondragstart(e, id)}
          ondragover={(e) => ondragover(e, id)}
          ondragleave={() => dropTarget === id && (dropTarget = null)}
          ondrop={(e) => ondrop(e, id)}
          {ondragend}
        >
          <span class="name">{name(id)}</span>
          <span class="move">
            <button
              type="button"
              class="btn subtle icon"
              data-move="up"
              aria-label={`Move ${name(id)} up`}
              title="Move up"
              disabled={index === 0}
              onclick={(e) => step(id, -1, e.currentTarget)}
            >
              <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 10l4-4 4 4" /></svg>
            </button>
            <button
              type="button"
              class="btn subtle icon"
              data-move="down"
              aria-label={`Move ${name(id)} down`}
              title="Move down"
              disabled={index === members.length - 1}
              onclick={(e) => step(id, 1, e.currentTarget)}
            >
              <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 6l4 4 4-4" /></svg>
            </button>
          </span>
          <button
            type="button"
            class="btn subtle"
            data-remove
            aria-label={`Remove ${name(id)} from group`}
            onclick={() => removeMember(id)}
          >
            Remove from group
          </button>
        </li>
      {/each}
      <li
        class="end"
        class:drop={dropTarget === END}
        aria-hidden="true"
        ondragover={(e) => ondragover(e, END)}
        ondragleave={() => dropTarget === END && (dropTarget = null)}
        ondrop={(e) => ondrop(e, END)}
      ></li>
    </ol>
  </section>

  <div class="actions">
    <button type="button" class="btn" onclick={dissolve}>
      <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M2.5 4.5h5v7h-5zM8.5 4.5h5v7h-5z" /></svg>
      Ungroup
    </button>
  </div>
</article>

<style>
  .editor {
    display: grid;
    gap: 16px;
    align-content: start;
  }

  header {
    display: grid;
    gap: 2px;
  }

  h2 {
    margin: 0;
    font-family: "Segoe UI Variable Display", "Segoe UI Variable", "Segoe UI", sans-serif;
    font-size: 28px;
    line-height: 36px;
    font-weight: 600;
  }

  h3 {
    margin: 0 0 8px;
    font-size: 14px;
    line-height: 20px;
    font-weight: 600;
  }

  header p,
  .notice {
    margin: 0;
  }

  .card {
    margin: 0;
    padding: 16px;
    border: 1px solid var(--card-stroke);
    border-radius: var(--radius-large);
    background: var(--card);
  }

  .form {
    display: grid;
    gap: 16px;
  }

  .field {
    display: grid;
    gap: 4px;
  }

  .hint {
    font-size: 12px;
    line-height: 16px;
    margin-top: -4px;
  }

  .interval {
    width: 160px;
  }

  .msg:empty {
    display: none;
  }

  .members {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 2px;
  }

  .members > li {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 2px 4px 2px 12px;
    border-radius: var(--radius);
    border-top: 2px solid transparent;
  }

  .members > li:hover {
    background: var(--subtle-hover);
  }

  .members > li.drop {
    border-top-color: var(--accent);
  }

  .members > li.dragging {
    opacity: 0.5;
  }

  .members > li.end {
    min-height: 12px;
    padding: 0;
  }

  .members > li.end:hover {
    background: none;
  }

  .name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .move {
    display: inline-flex;
    flex: none;
  }

  .move .btn {
    min-height: 28px;
    min-width: 28px;
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }
</style>
