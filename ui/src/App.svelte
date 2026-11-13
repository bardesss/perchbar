<script lang="ts">
  import { onMount } from "svelte";
  import { getState, onFocus, onStateChanged, sendReady, type CommandError, type StateView } from "./api";
  import AddWidgetDialog from "./lib/AddWidgetDialog.svelte";
  import AppSettings from "./lib/AppSettings.svelte";
  import GroupEditor from "./lib/GroupEditor.svelte";
  import { instanceLabels } from "./lib/form";
  import { latestRefresher } from "./lib/latest";
  import { keyedSavers } from "./lib/saving";
  import WidgetEditor from "./lib/WidgetEditor.svelte";
  import WidgetList from "./lib/WidgetList.svelte";

  let view: StateView | null = $state(null);
  let loadError: string | null = $state(null);
  let notice: string | null = $state(null);
  /** The instance or group shown in the main pane. */
  let selectedId: string | null = $state(null);
  /** Goes up on every state change; open logs refetch on it. */
  let changes = $state(0);
  let adding = $state(false);

  /** Instance ids in strip order, group members in place of their group. */
  const stripInstances = $derived.by(() => {
    if (!view) return [];
    const groups = new Map(view.groups.map((g) => [g.id, g.members]));
    const ids = view.order.flatMap((id) => groups.get(id) ?? [id]);
    const known = new Set(view.instances.map((i) => i.id));
    return ids.filter((id) => known.has(id));
  });
  // Numbered in the order they were added, so names stay put on reorder.
  const labels = $derived(view ? instanceLabels(view.instances, view.widgets) : {});
  const selected = $derived(view?.instances.find((i) => i.id === selectedId));
  const selectedWidget = $derived(view?.widgets.find((w) => w.id === selected?.widget));
  const selectedGroup = $derived(view?.groups.find((g) => g.id === selectedId));

  /** A focus request for an instance the state does not have yet. */
  let wantedId: string | null = null;

  /**
   * Refetches the state. Resolves once state at least as new as this fetch
   * is in, so callers that await it read what they just changed.
   */
  const refresh = latestRefresher(
    getState,
    (next) => {
      view = next;
      loadError = null;
      reconcileSelection();
    },
    (e) => (loadError = (e as CommandError).message),
  );

  /** Every change to one group waits for the one before it. */
  const forGroup = keyedSavers(() => refresh());
  let list: WidgetList | undefined = $state();

  function reconcileSelection() {
    if (!view) return;
    const has = (id: string | null) => id !== null && view!.instances.some((i) => i.id === id);
    const isGroup = (id: string | null) => id !== null && view!.groups.some((g) => g.id === id);
    if (wantedId && has(wantedId)) {
      selectedId = wantedId;
      wantedId = null;
    } else if (!has(selectedId) && !isGroup(selectedId)) {
      selectedId = stripInstances[0] ?? null;
    }
  }

  function select(id: string) {
    selectedId = id;
    notice = null;
  }

  function focusRequested(id: string) {
    if (view?.instances.some((i) => i.id === id)) {
      select(id);
    } else {
      wantedId = id;
      void refresh();
    }
  }

  async function stateChanged() {
    await refresh();
    changes++;
  }

  onMount(() => {
    const stops: (() => void)[] = [];
    let gone = false;
    const keep = (un: () => void) => (gone ? un() : stops.push(un));

    void refresh();
    Promise.all([onStateChanged(() => void stateChanged()), onFocus(focusRequested)]).then(
      ([a, b]) => {
        keep(a);
        keep(b);
        // Only now can a focus request be heard.
        if (!gone) void sendReady();
      },
      () => (loadError = "Cannot listen for changes from Perchbar"),
    );
    return () => {
      gone = true;
      for (const un of stops) un();
    };
  });
</script>

<div class="layout">
  <aside>
    <h1>On your taskbar</h1>
    <div class="list">
      {#if view}
        <WidgetList
          bind:this={list}
          {view}
          {forGroup}
          {labels}
          {selectedId}
          onselect={select}
          onchanged={refresh}
          onerror={(m) => (notice = m)}
        />
      {/if}
    </div>
    <div class="add">
      <button type="button" class="btn" disabled={!view} onclick={() => (adding = true)}>
        <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 3v10M3 8h10" /></svg>
        Add widget
      </button>
    </div>
    {#if view}
      <AppSettings autostart={view.autostart} version={view.version} onchanged={refresh} />
    {/if}
  </aside>

  <main>
    {#if notice}<p class="notice field-error" role="alert">{notice}</p>{/if}
    {#if loadError}
      <p class="field-error" role="alert">{loadError}</p>
    {:else if !view}
      <p class="muted">Loading</p>
    {:else if selectedGroup}
      {#key selectedGroup.id}
        {@const groupId = selectedGroup.id}
        <GroupEditor
          group={selectedGroup}
          current={() => view?.groups.find((g) => g.id === groupId)}
          saver={forGroup(groupId)}
          {labels}
          onselect={select}
          focusRow={(id) => list?.focusRow(id) ?? Promise.resolve()}
          onchanged={refresh}
        />
      {/key}
    {:else if selected}
      {#key selected.id}
        <WidgetEditor
          instance={selected}
          widget={selectedWidget}
          label={labels[selected.id] ?? selected.widget}
          {changes}
          onchanged={refresh}
          onremoved={() => (selectedId = null)}
        />
      {/key}
    {:else}
      <div class="empty">
        <p>Nothing on your taskbar yet.</p>
        <button type="button" class="btn accent" onclick={() => (adding = true)}>Add widget</button>
      </div>
    {/if}
  </main>
</div>

{#if view}
  <AddWidgetDialog
    open={adding}
    widgets={view.widgets}
    onchanged={refresh}
    onclose={() => (adding = false)}
    onadded={(id) => {
      adding = false;
      wantedId = id;
      void refresh();
    }}
  />
{/if}

<style>
  .layout {
    display: grid;
    grid-template-columns: minmax(260px, 320px) 1fr;
    height: 100%;
  }

  aside {
    display: grid;
    grid-template-rows: auto 1fr auto auto;
    min-height: 0;
  }

  h1 {
    margin: 0;
    padding: 16px 16px 8px;
    font-size: 14px;
    line-height: 20px;
    font-weight: 600;
  }

  .list {
    min-height: 0;
    overflow: auto;
    padding: 0 8px;
  }

  .add {
    padding: 8px 16px 12px;
  }

  main {
    min-height: 0;
    overflow: auto;
    padding: 24px 32px;
    background: var(--layer);
    border-left: 1px solid var(--layer-stroke);
    border-top: 1px solid var(--layer-stroke);
    border-top-left-radius: var(--radius-large);
  }

  .notice {
    margin: 0 0 12px;
  }

  .empty {
    display: grid;
    justify-items: start;
    gap: 12px;
  }

  .empty p {
    margin: 0;
  }
</style>
