<script lang="ts">
  // Lists the installed widgets; picking one adds it to the taskbar.
  // "Load from folder..." reads a widget folder and shows its permissions
  // before anything is installed. Text from the folder is untrusted and
  // goes through text bindings only.
  import {
    addInstance,
    approvePermissions,
    inspectFolder,
    installLocal,
    pickFolder,
    removeWidget,
    type CommandError,
    type FolderReport,
    type MissingLocalView,
    type WidgetView,
  } from "../api";
  import { reviewIntro } from "./permissions";
  import PermissionReview from "./PermissionReview.svelte";

  interface Props {
    open: boolean;
    widgets: WidgetView[];
    /** Local widgets whose folder is missing. */
    missing: MissingLocalView[];
    onadded: (id: string) => void;
    /** Refetches the state; resolves once the new state is in. */
    onchanged: () => Promise<void>;
    onclose: () => void;
  }

  let { open, widgets, missing, onadded, onchanged, onclose }: Props = $props();

  let dialog: HTMLDialogElement | undefined = $state();
  let error: string | null = $state(null);
  let busy = $state(false);
  /** The folder under review, if any. */
  let report: FolderReport | null = $state(null);
  /** The missing local widget whose removal waits for a second click. */
  let confirmRemove: string | null = $state(null);

  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) {
      error = null;
      report = null;
      confirmRemove = null;
      dialog.showModal();
    } else if (!open && dialog.open) {
      dialog.close();
    }
  });

  function meta(w: Pick<WidgetView, "author" | "version">): string {
    return [w.author ? `By ${w.author}` : "", w.version ? `version ${w.version}` : ""]
      .filter(Boolean)
      .join(", ");
  }

  async function loadFolder() {
    error = null;
    let path: string | null;
    try {
      path = await pickFolder();
    } catch {
      error = "Cannot open the folder picker";
      return;
    }
    if (path === null) return;
    busy = true;
    try {
      report = await inspectFolder(path);
    } catch (e) {
      error = (e as CommandError).message;
    } finally {
      busy = false;
    }
  }

  async function install() {
    if (!report) return;
    busy = true;
    error = null;
    try {
      await installLocal(report.folder, report.permissions);
      report = null;
      await onchanged();
    } catch (e) {
      error = (e as CommandError).message;
    } finally {
      busy = false;
    }
  }

  /** Lets a blocked widget run with what it asks; then it can be added. */
  async function allow(w: WidgetView) {
    busy = true;
    error = null;
    try {
      await approvePermissions(w.id, w.permissions);
      await onchanged();
    } catch (e) {
      error = (e as CommandError).message;
    } finally {
      busy = false;
    }
  }

  async function forget(id: string) {
    busy = true;
    error = null;
    try {
      await removeWidget(id);
      confirmRemove = null;
      await onchanged();
    } catch (e) {
      error = (e as CommandError).message;
    } finally {
      busy = false;
    }
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
  <h2 id="add-title">{report ? "Load from folder" : "Add widget"}</h2>
  {#if report}
    <div class="report">
      {#if report.name}
        <p class="name">{report.name}</p>
        {#if report.description}<p class="desc">{report.description}</p>{/if}
        <p class="meta">{meta(report)}</p>
      {/if}
      <p class="meta folder">{report.folder}</p>
      <p class="badge">Local: not reviewed</p>
      {#if report.errors.length > 0}
        <ul class="errors" role="alert">
          {#each report.errors as message, i (i)}
            <li class="field-error">{message}</li>
          {/each}
        </ul>
        {#if error}<p class="field-error" role="alert">{error}</p>{/if}
        <div class="footer">
          <button type="button" class="btn" onclick={() => (report = null)}>Back</button>
        </div>
      {:else}
        <PermissionReview
          permissions={report.permissions}
          confirmLabel="Install"
          {busy}
          {error}
          onconfirm={install}
          oncancel={() => {
            report = null;
            error = null;
          }}
        />
      {/if}
    </div>
  {:else if widgets.length === 0 && missing.length === 0}
    <p class="muted">No widgets are installed.</p>
  {:else}
    <ul>
      {#each widgets as w (w.id)}
        <li>
          {#if w.blocked}
            <div class="pick blocked">
              <span class="name">{w.name}</span>
              {#if w.description}<span class="desc">{w.description}</span>{/if}
              <span class="meta">{meta(w)}</span>
              {#if w.local}<span class="badge">Local: not reviewed</span>{/if}
              <PermissionReview
                permissions={w.added}
                intro={reviewIntro(w.name, w.firstReview)}
                confirmLabel="Allow"
                {busy}
                onconfirm={() => allow(w)}
              />
            </div>
          {:else}
            <button type="button" class="pick" disabled={busy} onclick={() => pick(w)}>
              <span class="name">{w.name}</span>
              {#if w.description}<span class="desc">{w.description}</span>{/if}
              <span class="meta">{meta(w)}</span>
              {#if w.local}<span class="badge">Local: not reviewed</span>{/if}
            </button>
          {/if}
        </li>
      {/each}
      {#each missing as m (m.id)}
        <li>
          <div class="pick blocked">
            <span class="name">{m.id}</span>
            <span class="field-error">Its folder is missing or cannot be loaded:</span>
            <span class="meta folder">{m.folder}</span>
            <span class="meta">Load the folder again from its new place, or remove the widget.</span>
            <span class="row">
              {#if confirmRemove === m.id}
                <button type="button" class="btn accent" disabled={busy} onclick={() => forget(m.id)}>
                  Remove widget
                </button>
                <button type="button" class="btn" disabled={busy} onclick={() => (confirmRemove = null)}>Cancel</button>
              {:else}
                <button type="button" class="btn danger" disabled={busy} onclick={() => (confirmRemove = m.id)}>
                  Remove widget
                </button>
              {/if}
            </span>
          </div>
        </li>
      {/each}
    </ul>
  {/if}
  {#if !report}
    {#if error}<p class="field-error" role="alert">{error}</p>{/if}
    <div class="footer">
      <button type="button" class="btn" disabled={busy} onclick={loadFolder}>Load from folder...</button>
      <button type="button" class="btn" onclick={() => dialog?.close()}>Cancel</button>
    </div>
  {/if}
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

  .pick.blocked {
    gap: 4px;
  }

  .row {
    display: flex;
    gap: 8px;
    margin-top: 4px;
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
    gap: 8px;
    margin-top: 16px;
  }

  .report {
    display: grid;
    gap: 8px;
  }

  .report p {
    margin: 0;
  }

  .folder {
    overflow-wrap: anywhere;
  }

  .errors {
    margin: 0;
    padding-left: 20px;
  }

  .badge {
    color: var(--text-muted);
    font-size: 12px;
    line-height: 16px;
    font-style: italic;
  }
</style>
