<script lang="ts">
  // One non-secret, non-list setting. The parent holds the value; this
  // reports edits (oninput) and when to save (oncommit: blur, Enter, or a
  // change for selects, toggles and the color swatch).
  import type { FormField, FormValue } from "./form";

  interface Props {
    field: FormField;
    value: FormValue;
    error: string | null;
    saved: boolean;
    oninput: (value: FormValue) => void;
    oncommit: () => void;
  }

  let { field, value, error, saved, oninput, oncommit }: Props = $props();

  const id = $derived(`setting-${field.key}`);
  const hintId = $derived(`${id}-hint`);
  const msgId = $derived(`${id}-msg`);
  const text = $derived(typeof value === "string" ? value : "");
  const swatch = $derived(/^#[0-9a-fA-F]{6}$/.test(text) ? text.toLowerCase() : "#000000");
  const hint = $derived(field.kind === "location" ? "City or place" : null);
  const describedBy = $derived([hint ? hintId : "", error || saved ? msgId : ""].filter(Boolean).join(" "));

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Enter") {
      e.preventDefault();
      oncommit();
    }
  }

  function inputText(e: Event) {
    oninput((e.currentTarget as HTMLInputElement).value);
  }
</script>

<div class="field" class:inline={field.kind === "toggle"}>
  <label for={id}>
    {field.label}{#if field.required}<span class="muted" aria-hidden="true"> *</span>{/if}
  </label>
  {#if hint}<span id={hintId} class="hint muted">{hint}</span>{/if}

  {#if field.kind === "toggle"}
    <input
      {id}
      class="switch"
      type="checkbox"
      role="switch"
      checked={value === true}
      aria-describedby={describedBy || undefined}
      onchange={(e) => {
        oninput((e.currentTarget as HTMLInputElement).checked);
        oncommit();
      }}
    />
  {:else if field.kind === "select"}
    <select
      {id}
      class="input"
      value={text}
      aria-invalid={error ? "true" : undefined}
      aria-describedby={describedBy || undefined}
      onchange={(e) => {
        oninput((e.currentTarget as HTMLSelectElement).value);
        oncommit();
      }}
    >
      {#if text === "" || !field.options.includes(text)}
        <option value="">Not set</option>
      {/if}
      {#each field.options as option (option)}
        <option value={option}>{option}</option>
      {/each}
    </select>
  {:else if field.kind === "color"}
    <div class="color">
      <input
        class="swatch"
        type="color"
        value={swatch}
        aria-label={`${field.label}: pick a color`}
        onchange={(e) => {
          oninput((e.currentTarget as HTMLInputElement).value);
          oncommit();
        }}
      />
      <input
        {id}
        class="input hex"
        type="text"
        spellcheck="false"
        autocomplete="off"
        maxlength="7"
        placeholder="#rrggbb"
        value={text}
        aria-invalid={error ? "true" : undefined}
        aria-describedby={describedBy || undefined}
        oninput={inputText}
        onblur={oncommit}
        {onkeydown}
      />
    </div>
  {:else}
    <input
      {id}
      class="input"
      type="text"
      inputmode={field.kind === "number" ? "decimal" : undefined}
      spellcheck={field.kind === "text" ? undefined : "false"}
      autocomplete="off"
      value={text}
      aria-invalid={error ? "true" : undefined}
      aria-describedby={describedBy || undefined}
      oninput={inputText}
      onblur={oncommit}
      {onkeydown}
    />
  {/if}

  <span id={msgId} class="msg" aria-live="polite">
    {#if error}<span class="field-error">{error}</span>{:else if saved}<span class="field-saved">Saved</span>{/if}
  </span>
</div>

<style>
  .field {
    display: grid;
    gap: 4px;
  }

  .field.inline {
    grid-template-columns: 1fr auto;
    align-items: center;
  }

  .field.inline .msg {
    grid-column: 1 / -1;
  }

  label {
    font-weight: 400;
  }

  .hint {
    font-size: 12px;
    line-height: 16px;
    margin-top: -4px;
  }

  .color {
    display: flex;
    gap: 8px;
    align-items: center;
  }

  .swatch {
    flex: none;
    width: 32px;
    height: 32px;
    padding: 2px;
    border: 1px solid var(--control-stroke);
    border-bottom-color: var(--control-stroke-bottom);
    border-radius: var(--radius);
    background: var(--control);
  }

  .swatch::-webkit-color-swatch-wrapper {
    padding: 0;
  }

  .swatch::-webkit-color-swatch {
    border: none;
    border-radius: 2px;
  }

  .hex {
    width: 120px;
    font-variant-numeric: tabular-nums;
  }

  .msg:empty {
    display: none;
  }
</style>
