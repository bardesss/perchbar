<script lang="ts">
  // Lists the installed widgets; picking one adds it to the taskbar.
  import { addInstance, type CommandError, type WidgetView } from "../api";

  interface Props {
    open: boolean;
    widgets: WidgetView[];
    onadded: (id: string) => void;
    onclose: () => void;
  }

  let { open, widgets, onadded, onclose }: Props = $props();

  let dialog: HTMLDialogElement | undefined = $state();
  let error: string | null = $state(null);
  let busy = $state(false);

  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) {
      error = null;
      dialog.showModal();
    } else if (!open && dialog.open) {
      dialog.close();
    }
  });

  function meta(w: WidgetView): string {
    return [w.author ? `By ${w.author}` : "", w.version ? `version ${w.version}` : ""]
      .filter(Boolean)
      .join(", ");
  }

  async function pick(w: WidgetView) {
    busy = true;
    error = null;
    try {
      const id = await addInstance(w.id);
      onadded(id);
    } catch (e) {
      error = (e as CommandError).message;
    } finally {
      busy = false;
    }
  }
</script>

<dialog bind:this={dialog} aria-labelledby="add-title" onclose={onclose}>
  <h2 id="add-title">Add widget</h2>
  {#if widgets.length === 0}
    <p class="muted">No widgets are installed.</p>
  {:else}
    <ul>
      {#each widgets as w (w.id)}
        <li>
          <button type="button" class="pick" disabled={busy || w.blocked !== null} onclick={() => pick(w)}>
            <span class="name">{w.name}</span>
            {#if w.description}<span class="desc">{w.description}</span>{/if}
            <span class="meta">{meta(w)}</span>
            {#if w.blocked}<span class="field-error">{w.blocked}</span>{/if}
          </button>
        </li>
      {/each}
    </ul>
  {/if}
  {#if error}<p class="field-error" role="alert">{error}</p>{/if}
  <div class="footer">
    <button type="button" class="btn" onclick={() => dialog?.close()}>Cancel</button>
  </div>
</dialog>

<style>
  dialog {
    width: min(480px, calc(100vw - 48px));
    max-height: calc(100vh - 48px);
    padding: 24px;
    border: 1px solid var(--card-stroke);
    border-radius: var(--radius-large);
    background: var(--solid-bg);
    color: var(--text);
    box-shadow: 0 32px 64px rgb(0 0 0 / 0.19), 0 2px 21px rgb(0 0 0 / 0.15);
  }

  dialog::backdrop {
    background: rgb(0 0 0 / 0.3);
  }

  h2 {
    margin: 0 0 12px;
    font-size: 20px;
    line-height: 28px;
    font-weight: 600;
  }

  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 4px;
  }

  .pick {
    width: 100%;
    display: grid;
    gap: 2px;
    padding: 8px 12px;
    border: 1px solid var(--card-stroke);
    border-radius: var(--radius);
    background: var(--card);
    text-align: left;
    cursor: default;
  }

  .pick:hover:not(:disabled) {
    background: var(--subtle-hover);
  }

  .pick:disabled {
    color: var(--text-disabled);
  }

  .name {
    font-weight: 600;
  }

  .desc,
  .meta {
    color: var(--text-muted);
    font-size: 12px;
    line-height: 16px;
  }

  .footer {
    display: flex;
    justify-content: flex-end;
    margin-top: 16px;
  }
</style>
