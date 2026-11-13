<script lang="ts">
  // "On your taskbar": instances and groups in strip order. Top-level rows
  // reorder by drag (onto the top or bottom third of a row, or after the
  // last row) or with Move up / Move down. A single row dropped onto the
  // middle of another single row makes a group of the two, onto a group row
  // joins that group; "Group with..." does the same from the keyboard. A
  // group row opens the group editor and shows its members under it.
  import { tick } from "svelte";
  import {
    createGroup,
    setHidden,
    setOrder,
    updateGroup,
    type CommandError,
    type StateView,
  } from "../api";
  import { NEW_GROUP_INTERVAL, statusLine } from "./form";
  import { dropAction, dropZone, moveBy, type DropZone } from "./order";
  import type { Saver } from "./saving";

  interface Props {
    view: StateView;
    labels: Record<string, string>;
    /** The instance or group shown in the main pane. */
    selectedId: string | null;
    onselect: (id: string) => void;
    onchanged: () => Promise<void>;
    onerror: (message: string) => void;
    /** The queue every change to one group goes through. */
    forGroup: (id: string) => Saver;
  }

  let { view, labels, selectedId, onselect, onchanged, onerror, forGroup }: Props = $props();

  /** The drop zone after the last row. Ids are never empty. */
  const END = "";

  /** Order shown until the refetch after a move lands. */
  let pendingOrder: string[] | null = $state(null);
  let dragging: string | null = $state(null);
  /** Where a drop would land on a row. */
  let drop: { id: string; zone: DropZone } | null = $state(null);
  /** The row whose "Group with..." choices are open. */
  let menuFor: string | null = $state(null);

  const order = $derived(pendingOrder ?? view.order);
  const instances = $derived(new Map(view.instances.map((i) => [i.id, i])));
  const groups = $derived(new Map(view.groups.map((g) => [g.id, g])));
  const singles = $derived(order.filter((id) => !groups.has(id)));
  const groupIds = $derived(order.filter((id) => groups.has(id)));
  const isGroup = (id: string) => groups.has(id);

  function groupName(id: string): string {
    return `Group of ${groups.get(id)?.members.length ?? 0}`;
  }

  function rowName(id: string): string {
    return isGroup(id) ? groupName(id) : (labels[id] ?? id);
  }

  /** Focuses `part` of a top-level row once the page shows the change. */
  export async function focusRow(id: string, part = ".select") {
    await tick();
    document.querySelector<HTMLElement>(`li[data-id="${CSS.escape(id)}"] ${part}`)?.focus();
  }

  async function apply(next: string[] | null) {
    if (!next) return;
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

  /** Groups two single rows; the new group opens in the main pane. */
  async function makeGroup(members: [string, string]) {
    try {
      const id = await createGroup(members, NEW_GROUP_INTERVAL);
      await onchanged();
      onselect(id);
      await focusRow(id);
    } catch (e) {
      onerror((e as CommandError).message);
    }
  }

  /**
   * Adds a single row to the end of a group, which opens in the main pane.
   * Waits its turn in the group's queue and reads the group only then.
   */
  function joinGroup(id: string, groupId: string): Promise<void> {
    return forGroup(groupId).run(async () => {
      const group = groups.get(groupId);
      if (!group || group.members.includes(id)) return;
      try {
        await updateGroup(groupId, [...group.members, id], group.interval);
        await onchanged();
        onselect(groupId);
        await focusRow(groupId);
      } catch (e) {
        onerror((e as CommandError).message);
      }
    });
  }

  async function step(id: string, delta: -1 | 1, button: HTMLElement) {
    await apply(moveBy(order, id, delta));
    await tick();
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

  async function toggleMenu(id: string) {
    menuFor = menuFor === id ? null : id;
    if (menuFor) await focusRow(id, ".menu button");
  }

  function closeMenu(id: string) {
    menuFor = null;
    void focusRow(id, "[data-group-with]");
  }

  function menuKey(e: KeyboardEvent, id: string) {
    if (e.key === "Escape") {
      e.preventDefault();
      closeMenu(id);
    }
  }

  function choose(id: string, other: string) {
    menuFor = null;
    if (isGroup(other)) void joinGroup(id, other);
    else void makeGroup([id, other]);
  }

  function ondragstart(e: DragEvent, id: string) {
    dragging = id;
    menuFor = null;
    e.dataTransfer?.setData("text/plain", id);
    // "link" shows a different cursor while a drop would group.
    if (e.dataTransfer) e.dataTransfer.effectAllowed = "linkMove";
  }

  function zoneAt(e: DragEvent, from: string, target: string): DropZone {
    if (target === END) return "before";
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    return dropZone(e.clientY - rect.top, rect.height, from !== target && !isGroup(from));
  }

  function ondragover(e: DragEvent, id: string) {
    if (!dragging) return;
    e.preventDefault();
    const zone = zoneAt(e, dragging, id);
    if (e.dataTransfer) e.dataTransfer.dropEffect = zone === "into" ? "link" : "move";
    drop = { id, zone };
  }

  function ondragleave(id: string) {
    if (drop?.id === id) drop = null;
  }

  function ondrop(e: DragEvent, id: string) {
    if (!dragging) return;
    e.preventDefault();
    const from = dragging;
    const zone = zoneAt(e, from, id);
    dragging = null;
    drop = null;
    const action = dropAction(order, isGroup, from, id === END ? null : id, zone);
    if (!action) return;
    if (action.kind === "order") void apply(action.order);
    else if (action.kind === "create") void makeGroup(action.members);
    else void joinGroup(from, action.group);
  }

  function ondragend() {
    dragging = null;
    drop = null;
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
        >{status || "\u00a0"}</span>
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

{#snippet groupRow(id: string)}
  {@const group = groups.get(id)}
  {#if group}
    <div class="row" class:selected={selectedId === id}>
      <button
        type="button"
        class="select"
        aria-current={selectedId === id ? "true" : undefined}
        onclick={() => onselect(id)}
      >
        <span class="name">{groupName(id)}</span>
        <span class="status">Rotates every {group.interval} s</span>
      </button>
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
      onclick={(e) => step(id, -1, e.currentTarget)}
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
      onclick={(e) => step(id, 1, e.currentTarget)}
    >
      <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 6l4 4 4-4" /></svg>
    </button>
  </span>
{/snippet}

{#snippet groupWith(id: string, name: string)}
  {@const none = singles.length < 2 && groupIds.length === 0}
  <button
    type="button"
    class="btn subtle icon"
    data-group-with
    aria-label={`Group ${name} with...`}
    aria-expanded={menuFor === id}
    aria-controls={menuFor === id ? `group-with-${id}` : undefined}
    title={none ? "Nothing to group with" : "Group with..."}
    disabled={none}
    onclick={() => toggleMenu(id)}
  >
    <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M2.5 3.5h7v5h-7zM6.5 8.5v4h7v-5h-4" /></svg>
  </button>
{/snippet}

{#snippet groupMenu(id: string, name: string)}
  <div
    class="menu"
    id={`group-with-${id}`}
    role="group"
    aria-label={`Group ${name} with`}
  >
    <span class="menu-title muted">Group with</span>
    {#each singles.filter((o) => o !== id) as other (other)}
      <button type="button" class="btn subtle item" onkeydown={(e) => menuKey(e, id)} onclick={() => choose(id, other)}>
        {rowName(other)}
      </button>
    {/each}
    {#each groupIds as gid (gid)}
      {@const members = groups.get(gid)?.members ?? []}
      <button type="button" class="btn subtle item" onkeydown={(e) => menuKey(e, id)} onclick={() => choose(id, gid)}>
        {groupName(gid)}: {members.map((m) => labels[m] ?? m).join(", ")}
      </button>
    {/each}
    <button type="button" class="btn subtle item" onkeydown={(e) => menuKey(e, id)} onclick={() => closeMenu(id)}>Cancel</button>
  </div>
{/snippet}

{#if order.length === 0}
  <p class="empty muted">Nothing on your taskbar yet.</p>
{:else}
  <ol class="list" aria-label="On your taskbar">
    {#each order as id, index (id)}
      {@const group = groups.get(id)}
      {@const name = rowName(id)}
      <li
        data-id={id}
        draggable="true"
        class:dragging={dragging === id}
        class:drop={drop?.id === id && drop.zone === "before" && dragging !== id}
        class:after={drop?.id === id && drop.zone === "after" && dragging !== id}
        class:into={drop?.id === id && drop.zone === "into"}
        ondragstart={(e) => ondragstart(e, id)}
        ondragover={(e) => ondragover(e, id)}
        ondragleave={() => ondragleave(id)}
        ondrop={(e) => ondrop(e, id)}
        {ondragend}
      >
        {#if group}
          <div class="top">
            {@render groupRow(id)}
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
            {@render groupWith(id, name)}
            {@render moveButtons(id, index, name)}
          </div>
          {#if menuFor === id}
            {@render groupMenu(id, name)}
          {/if}
        {/if}
      </li>
    {/each}
    <li
      class="end"
      class:drop={drop?.id === END}
      aria-hidden="true"
      ondragover={(e) => ondragover(e, END)}
      ondragleave={() => ondragleave(END)}
      ondrop={(e) => ondrop(e, END)}
    ></li>
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
    border-bottom: 2px solid transparent;
  }

  .list > li.drop {
    border-top-color: var(--accent);
  }

  .list > li.after {
    border-bottom-color: var(--accent);
  }

  .list > li.into {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }

  .list > li.end {
    min-height: 16px;
  }

  .list > li.dragging {
    opacity: 0.5;
  }

  .top {
    display: flex;
    align-items: center;
    gap: 2px;
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

  .top > .btn,
  .move .btn {
    min-height: 28px;
    min-width: 28px;
  }

  .menu {
    display: grid;
    gap: 2px;
    margin: 2px 0 6px 16px;
    padding: 4px;
    border: 1px solid var(--card-stroke);
    border-radius: var(--radius-large);
    background: var(--card);
  }

  .menu-title {
    padding: 2px 8px;
    font-size: 12px;
    line-height: 16px;
  }

  .menu .item {
    justify-content: flex-start;
    text-align: left;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .empty {
    margin: 0;
    padding: 8px 12px;
  }
</style>
