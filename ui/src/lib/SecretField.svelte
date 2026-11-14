<script lang="ts">
  // A secret setting. The page never sees a stored value: it only knows
  // whether one is saved. A typed value is sent with set_secret and then
  // dropped from the input. Saves on Enter or with the Save button (not on
  // blur, so a half-typed key is never stored by tabbing away).
  import { tick } from "svelte";
  import { clearSecret, setSecret, type CommandError, type SettingField } from "../api";
  import { checkField, errorsByKey } from "./form";

  interface Props {
    instanceId: string;
    field: SettingField;
    isSet: boolean;
    unreadable: boolean;
    /** The core's message for this key from a refused settings save. */
    error: string | null;
    /** Called after a secret is saved, so that message can go. */
    onsaved: () => void;
    onchanged: () => Promise<void>;
  }

  let { instanceId, field, isSet, unreadable, error: coreError, onsaved, onchanged }: Props = $props();

  let replacing = $state(false);
  let value = $state("");
  let error: string | null = $state(null);
  let busy = $state(false);
  let input: HTMLInputElement | undefined = $state();
  let replaceButton: HTMLButtonElement | undefined = $state();

  const id = $derived(`secret-${field.key}`);
  const editing = $derived(!isSet || replacing);
  const hint = $derived(
    unreadable && !isSet ? "Re-enter this secret" : field.required && !isSet ? "Required" : null,
  );

  async function save() {
    const problem = checkField(field, value);
    if (problem) {
      error = problem;
      return;
    }
    busy = true;
    let ok = false;
    try {
      await setSecret(instanceId, field.key, value);
      value = "";
      error = null;
      replacing = false;
      ok = true;
      onsaved();
      await onchanged();
    } catch (e) {
      const err = e as CommandError;
      error = errorsByKey(err)[field.key] ?? err.message;
    } finally {
      busy = false;
    }
    if (ok) {
      // The "Saved" label that replaces the input confirms it; keyboard
      // focus moves to Replace, since the input is gone.
      await tick();
      replaceButton?.focus();
    }
  }

  async function remove() {
    busy = true;
    let ok = false;
    try {
      await clearSecret(instanceId, field.key);
      error = null;
      ok = true;
      await onchanged();
    } catch (e) {
      error = (e as CommandError).message;
    } finally {
      busy = false;
    }
    if (ok) {
      await tick();
      input?.focus();
    }
  }

  async function startReplace() {
    replacing = true;
    error = null;
    await tick();
    input?.focus();
  }

  function cancel() {
    replacing = false;
    value = "";
    error = null;
  }
</script>

<div class="field">
  <label for={id}>
    {field.label}{#if field.required}<span class="muted" aria-hidden="true"> *</span>{/if}
  </label>
  {#if editing}
    <div class="row">
      <input
        {id}
        bind:this={input}
        class="input"
        type="password"
        autocomplete="off"
        spellcheck="false"
        bind:value
        aria-invalid={error || coreError ? "true" : undefined}
        aria-describedby={`${id}-msg`}
        onkeydown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            void save();
          } else if (e.key === "Escape" && replacing) {
            cancel();
          }
        }}
      />
      <button type="button" class="btn accent" disabled={busy || value === ""} onclick={save}>Save</button>
      {#if replacing}
        <button type="button" class="btn" disabled={busy} onclick={cancel}>Cancel</button>
      {/if}
    </div>
  {:else}
    <div class="row">
      <span class="saved-chip" id={id}>
        <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3.5 8.5l3 3 6-7" /></svg>
        Saved
      </span>
      <button type="button" class="btn" bind:this={replaceButton} disabled={busy} onclick={startReplace}>
        Replace
      </button>
      <button type="button" class="btn" disabled={busy} onclick={remove}>Remove</button>
    </div>
  {/if}
  <span id={`${id}-msg`} class="msg" aria-live="polite">
    {#if error || coreError}
      <span class="field-error">{error ?? coreError}</span>
    {:else if hint}
      <span class={hint === "Required" ? "muted" : "field-error"}>{hint}</span>
    {/if}
  </span>
</div>

<style>
  .field {
    display: grid;
    gap: 4px;
  }

  .row {
    display: flex;
    gap: 8px;
    align-items: center;
  }

  .row .input {
    flex: 1;
  }

  .saved-chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    flex: 1;
    min-height: 32px;
    color: var(--text-muted);
  }

  .saved-chip svg {
    width: 16px;
    height: 16px;
    fill: none;
    stroke: var(--success);
    stroke-width: 1.5;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .msg {
    font-size: 12px;
    line-height: 16px;
  }

  .msg:empty {
    display: none;
  }
</style>
