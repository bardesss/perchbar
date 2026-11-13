<script lang="ts">
  // "On your taskbar": instances and groups in strip order. Top-level rows
  // reorder by drag or with Move up / Move down; group members show under
  // their group (group editing comes with the groups UI).
  import { setHidden, setOrder, type CommandError, type StateView } from "../api";
  import { statusLine } from "./form";

  interface Props {
    view: StateView;
    labels: Record<string, string>;
    selectedId: string | null;
    onselect: (id: string) => void;
    onchanged: () => Promise<void>;
    onerror: (message: string) => void;
  }

  let { view, labels, selectedId, onselect, onchanged, onerror }: Props = $props();

  /** Order shown until the refetch after a move lands. */
  let pendingOrder: string[] | null = $state(null);
  let dragging: string | null = $state(null);
  let dropTarget: string | null = $state(null);

  const order = $derived(pendingOrder ?? view.order);
  const instances = $derived(new Map(view.instances.map((i) => [i.id, i])));
  const groups = $derived(new Map(view.groups.map((g) => [g.id, g])));

  async function move(id: string, to: number) {
    const from = order.indexOf(id);
    if (from < 0 || to < 0 || to >= order.length || to === from) return;
    const next = order.slice();
    next.splice(from, 1);
    next.splice(to, 0, id);
    pendingOrder = next;
    try {
      await setOrder(next);
      await onchanged();
    } catch (e) {
      onerror((e as CommandError).message);
    } finally {
      pendingOrder = null;
    }
  }

  async function moveBy(id: string, delta: number, button: HTMLElement) {
    await move(id, order.indexOf(id) + delta);
    // The row moved; keep focus on the same button of the same row.
    const row = button.closest("li[data-id]");
    const name = button.dataset.move;
    const again = document.querySelector<HTMLButtonElement>(`li[data-id="${CSS.escape(id)}"] [data-move="${name}"]`);
    if (again && !again.disabled) again.focus();
    else row?.querySelector<HTMLButtonElement>("button")?.focus();
  }

  async function toggleHidden(id: string, hidden: boolean) {
    try {
      await setHidden(id, hidden);
      await onchanged();
    } catch (e) {
      onerror((e as CommandError).message);
    }
  }

  function ondragstart(e: DragEvent, id: string) {
    dragging = id;
    e.dataTransfer?.setData("text/plain", id);
    if (e.dataTransfer) e.dataTransfer.effectAllowed = "move";
  }

  function ondragover(e: DragEvent, id: string) {
    if (!dragging) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
    dropTarget = id;
  }

  function ondrop(e: DragEvent, id: string) {
    e.preventDefault();
    const from = dragging;
    dragging = null;
    dropTarget = null;
    if (from && from !== id) void move(from, order.indexOf(id));
  }

  function ondragend() {
    dragging = null;
    dropTarget = null;
  }
</script>

{#snippet instanceRow(id: string, nested: boolean)}
  {@const inst = instances.get(id)}
  {#if inst}
    {@const name = labels[id] ?? inst.widget}
    {@const status = statusLine(inst)}
    <div class="row" class:nested class:selected={selectedId === id}>
      <button
        type="button"
        class="select"
        aria-current={selectedId === id ? "true" : undefined}
        onclick={() => onselect(id)}
      >
        <span class="name">{name}</span>
        <span
          class="status"
          class:error={inst.status.state === "error" || inst.status.state === "blocked"}
        >{status || " "}</span>
      </button>
      <input
        class="switch"
        type="checkbox"
        role="switch"
        checked={!inst.hidden}
        aria-label={`Show ${name} on the taskbar`}
        title={inst.hidden ? "Hidden" : "Shown"}
        onchange={(e) => toggleHidden(id, !(e.currentTarget as HTMLInputElement).checked)}
      />
    </div>
  {/if}
{/snippet}

{#snippet moveButtons(id: string, index: number, name: string)}
  <span class="move">
    <button
      type="button"
      class="btn subtle icon"
      data-move="up"
      aria-label={`Move ${name} up`}
      title="Move up"
      disabled={index === 0}
      onclick={(e) => moveBy(id, -1, e.currentTarget)}
    >
      <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 10l4-4 4 4" /></svg>
    </button>
    <button
      type="button"
      class="btn subtle icon"
      data-move="down"
      aria-label={`Move ${name} down`}
      title="Move down"
      disabled={index === order.length - 1}
      onclick={(e) => moveBy(id, 1, e.currentTarget)}
    >
      <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 6l4 4 4-4" /></svg>
    </button>
  </span>
{/snippet}

{#if order.length === 0}
  <p class="empty muted">Nothing on your taskbar yet.</p>
{:else}
  <ol class="list" aria-label="On your taskbar">
    {#each order as id, index (id)}
      {@const group = groups.get(id)}
      {@const name = group ? `Group of ${group.members.length}` : (labels[id] ?? id)}
      <li
        data-id={id}
        draggable="true"
        class:dragging={dragging === id}
        class:drop={dropTarget === id && dragging !== id}
        ondragstart={(e) => ondragstart(e, id)}
        ondragover={(e) => ondragover(e, id)}
        ondragleave={() => dropTarget === id && (dropTarget = null)}
        ondrop={(e) => ondrop(e, id)}
        {ondragend}
      >
        {#if group}
          <div class="group-head">
            <span class="group-text">
              <span class="name">{name}</span>
              <span class="status">Rotates every {group.interval} s</span>
            </span>
            {@render moveButtons(id, index, name)}
          </div>
          <ul class="members" aria-label={name}>
            {#each group.members as member (member)}
              <li>{@render instanceRow(member, true)}</li>
            {/each}
          </ul>
        {:else}
          <div class="top">
            {@render instanceRow(id, false)}
            {@render moveButtons(id, index, name)}
          </div>
        {/if}
      </li>
    {/each}
  </ol>
{/if}

<style>
  .list,
  .members {
    list-style: none;
    margin: 0;
    padding: 0;
  }

  .list {
    display: grid;
    gap: 2px;
  }

  .list > li {
    border-radius: var(--radius);
    border-top: 2px solid transparent;
  }

  .list > li.drop {
    border-top-color: var(--accent);
  }

  .list > li.dragging {
    opacity: 0.5;
  }

  .top,
  .group-head {
    display: flex;
    align-items: center;
    gap: 2px;
  }

  .group-head {
    padding: 4px 4px 0 12px;
  }

  .group-text {
    flex: 1;
    min-width: 0;
    display: grid;
  }

  .row {
    position: relative;
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: 8px;
    padding-right: 8px;
    border-radius: var(--radius);
  }

  .row:hover {
    background: var(--subtle-hover);
  }

  .row.selected {
    background: var(--selected);
  }

  /* Windows 11 selection pill. */
  .row.selected::before {
    content: "";
    position: absolute;
    left: 0;
    top: 50%;
    width: 3px;
    height: 16px;
    margin-top: -8px;
    border-radius: 2px;
    background: var(--accent);
  }

  .row.nested {
    margin-left: 16px;
  }

  .select {
    flex: 1;
    min-width: 0;
    display: grid;
    padding: 6px 12px;
    border: none;
    border-radius: var(--radius);
    background: transparent;
    text-align: left;
    cursor: default;
  }

  .name,
  .status {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .status {
    color: var(--text-muted);
    font-size: 12px;
    line-height: 16px;
  }

  .status.error {
    color: var(--danger);
  }

  .move {
    display: inline-flex;
    flex: none;
  }

  .move .btn {
    min-height: 28px;
    min-width: 28px;
  }

  .empty {
    margin: 0;
    padding: 8px 12px;
  }
</style>
