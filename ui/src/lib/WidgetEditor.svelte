<script lang="ts">
  // Settings for one instance. The parent re-creates this per instance, so
  // drafts (typed but not saved) belong to the instance shown. A state
  // refresh updates the saved values but keeps every draft.
  import {
    refreshNow,
    removeInstance,
    setMaxWidth,
    updateSettings,
    type CommandError,
    type InstanceView,
    type SettingField as Field,
    type WidgetView,
  } from "../api";
  import {
    checkField,
    errorsByKey,
    parseMaxWidth,
    saveErrorMessage,
    settingsPayload,
    statusLine,
    toFormModel,
    toWire,
    type FormValue,
  } from "./form";
  import ListField from "./ListField.svelte";
  import LogView from "./LogView.svelte";
  import SecretField from "./SecretField.svelte";
  import SettingField from "./SettingField.svelte";
  import { createSaver, type Slot } from "./saving";

  interface Props {
    instance: InstanceView;
    widget: WidgetView | undefined;
    label: string;
    /** Goes up on every state change. */
    changes: number;
    /** Refetches the state; resolves once the new state is in. */
    onchanged: () => Promise<void>;
    onremoved: () => void;
  }

  let { instance, widget, label, changes, onchanged, onremoved }: Props = $props();

  const fields = $derived(widget?.settings ?? []);
  const model = $derived(toFormModel(fields, instance.settings));
  // Setting state, keyed by setting key.
  let drafts: Record<string, FormValue> = $state({});
  let errors: Record<string, string> = $state({});
  let saved: Record<string, boolean> = $state({});
  // Maximum width state, apart from the settings so no setting key can
  // collide with it. The draft is the text typed; empty means automatic.
  let width: { draft: string | null; error: string | null; saved: boolean } = $state({
    draft: null,
    error: null,
    saved: false,
  });
  let notice: string | null = $state(null);
  let confirmRemove = $state(false);
  let busy = $state(false);

  function settingSlot(key: string): Slot {
    return {
      draft: () => (key in drafts ? drafts[key] : undefined),
      clearDraft: () => delete drafts[key],
      setError: (m) => (m === null ? delete errors[key] : (errors[key] = m)),
      setSaved: (on) => (saved[key] = on),
    };
  }

  const widthSlot: Slot = {
    draft: () => width.draft ?? undefined,
    clearDraft: () => (width.draft = null),
    setError: (m) => (width.error = m),
    setSaved: (on) => (width.saved = on),
  };

  // Saves run one at a time, so each sends the state the last one left.
  const { save } = createSaver(() => onchanged());

  function shown(key: string, fallback: FormValue): FormValue {
    return key in drafts ? drafts[key] : fallback;
  }

  function edit(field: Field, value: FormValue) {
    drafts[field.key] = value;
    saved[field.key] = false;
    const problem = checkField(field, value);
    if (problem) errors[field.key] = problem;
    else delete errors[field.key];
  }

  function commit(field: Field) {
    const key = field.key;
    if (!(key in drafts)) return;
    const value = drafts[key];
    const problem = checkField(field, value);
    if (problem) {
      errors[key] = problem;
      return;
    }
    save(
      `setting:${key}`,
      settingSlot(key),
      value,
      () =>
        updateSettings(instance.id, settingsPayload(fields, instance.settings, { [key]: toWire(field, value) })),
      (err) => {
        // Problems in other fields (a missing secret too) show there.
        for (const [k, message] of Object.entries(errorsByKey(err))) if (k !== "") errors[k] = message;
        errors[key] = saveErrorMessage(fields, key, err);
      },
    );
  }

  const savedWidth = $derived(instance.maxWidth === null ? "" : String(instance.maxWidth));

  function editWidth(raw: string) {
    width.draft = raw;
    width.saved = false;
    width.error = parseMaxWidth(raw).error;
  }

  function commitWidth() {
    if (width.draft === null) return;
    const raw = width.draft;
    const { value, error } = parseMaxWidth(raw);
    if (error) {
      width.error = error;
      return;
    }
    save(
      "maxWidth",
      widthSlot,
      raw,
      () => setMaxWidth(instance.id, value),
      (err) => (width.error = err.message),
    );
  }

  async function refresh() {
    notice = null;
    try {
      await refreshNow(instance.id);
    } catch (e) {
      notice = (e as CommandError).message;
    }
  }

  async function remove() {
    busy = true;
    notice = null;
    try {
      await removeInstance(instance.id);
      onremoved();
      await onchanged();
    } catch (e) {
      notice = (e as CommandError).message;
      confirmRemove = false;
    } finally {
      busy = false;
    }
  }

  const status = $derived(statusLine(instance));
</script>

<article class="editor" aria-labelledby="editor-title">
  <header>
    <h2 id="editor-title">{label}</h2>
    {#if widget?.description}<p class="muted">{widget.description}</p>{/if}
    {#if status}
      <p class="status" class:error={instance.status.state === "error" || instance.status.state === "blocked"}>
        {status}
      </p>
    {/if}
  </header>

  {#if notice}<p class="notice field-error" role="alert">{notice}</p>{/if}

  {#if !widget}
    <p class="card muted">This widget is not installed, so its settings cannot be shown.</p>
  {:else if fields.length > 0}
    <section class="card form" aria-label="Settings">
      {#each model as f, i (f.key)}
        {@const field = fields[i]}
        {#if f.kind === "secret"}
          <SecretField
            instanceId={instance.id}
            {field}
            isSet={instance.secretsSet.includes(f.key)}
            unreadable={instance.secretsUnreadable}
            error={errors[f.key] ?? null}
            onsaved={() => delete errors[f.key]}
            {onchanged}
          />
        {:else if f.kind === "list"}
          <ListField
            field={f}
            value={shown(f.key, f.value) as string[]}
            error={errors[f.key] ?? null}
            saved={saved[f.key] ?? false}
            oninput={(v) => edit(field, v)}
            oncommit={() => commit(field)}
          />
        {:else}
          <SettingField
            field={f}
            value={shown(f.key, f.value)}
            error={errors[f.key] ?? null}
            saved={saved[f.key] ?? false}
            oninput={(v) => edit(field, v)}
            oncommit={() => commit(field)}
          />
        {/if}
      {/each}
    </section>
  {/if}

  <section class="card form" aria-label="Layout">
    <div class="field">
      <label for="max-width">Maximum width</label>
      <span id="max-width-hint" class="hint muted">Pixels, 40 to 600. Leave empty for automatic.</span>
      <input
        id="max-width"
        class="input width"
        type="text"
        inputmode="numeric"
        autocomplete="off"
        placeholder="Automatic"
        value={width.draft ?? savedWidth}
        aria-invalid={width.error ? "true" : undefined}
        aria-describedby="max-width-hint max-width-msg"
        oninput={(e) => editWidth((e.currentTarget as HTMLInputElement).value)}
        onblur={commitWidth}
        onkeydown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            commitWidth();
          }
        }}
      />
      <span id="max-width-msg" class="msg" aria-live="polite">
        {#if width.error}
          <span class="field-error">{width.error}</span>
        {:else if width.saved}
          <span class="field-saved">Saved</span>
        {/if}
      </span>
    </div>
  </section>

  <div class="actions">
    {#if widget}
      <button type="button" class="btn" onclick={refresh}>
        <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M13 8a5 5 0 1 1-1.5-3.5M13 2.5v3h-3" /></svg>
        Refresh now
      </button>
    {/if}
    {#if confirmRemove}
      <span class="confirm" role="group" aria-label="Confirm removal">
        <span>Remove {label} from the taskbar?</span>
        <button type="button" class="btn accent" disabled={busy} onclick={remove}>Remove</button>
        <button type="button" class="btn" disabled={busy} onclick={() => (confirmRemove = false)}>Cancel</button>
      </span>
    {:else}
      <button type="button" class="btn danger" onclick={() => (confirmRemove = true)}>
        <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3 4.5h10M6.5 4.5V3h3v1.5M4.5 4.5l.5 8.5h6l.5-8.5" /></svg>
        Remove from taskbar
      </button>
    {/if}
  </div>

  <LogView instanceId={instance.id} {changes} />
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
    overflow-wrap: anywhere;
  }

  header p {
    margin: 0;
  }

  .status {
    color: var(--text-muted);
    font-size: 12px;
  }

  .status.error {
    color: var(--danger);
  }

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

  .width {
    width: 160px;
  }

  .msg:empty {
    display: none;
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    align-items: center;
  }

  .confirm {
    display: inline-flex;
    flex-wrap: wrap;
    gap: 8px;
    align-items: center;
  }
</style>
