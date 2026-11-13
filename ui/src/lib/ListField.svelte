<script lang="ts">
  // A list setting: one text row per entry, with add and remove. Saves when
  // focus leaves the list, on Enter, and right after a row is removed.
  import { tick } from "svelte";
  import { LIST_MAX_ENTRIES, type FormField } from "./form";

  interface Props {
    field: FormField;
    value: string[];
    error: string | null;
    saved: boolean;
    oninput: (value: string[]) => void;
    oncommit: () => void;
  }

  let { field, value, error, saved, oninput, oncommit }: Props = $props();

  let box: HTMLDivElement | undefined = $state();
  const id = $derived(`setting-${field.key}`);
  const full = $derived(value.filter((v) => v.trim() !== "").length >= LIST_MAX_ENTRIES);

  function setRow(i: number, text: string) {
    const next = value.slice();
    next[i] = text;
    oninput(next);
  }

  function removeRow(i: number) {
    oninput(value.filter((_, j) => j !== i));
    oncommit();
    // Keep keyboard focus in the list.
    void tick().then(() => {
      const inputs = box?.querySelectorAll<HTMLInputElement>("input");
      if (!inputs || inputs.length === 0) box?.querySelector<HTMLButtonElement>(".add")?.focus();
      else inputs[Math.min(i, inputs.length - 1)].focus();
    });
  }

  async function addRow() {
    oninput([...value, ""]);
    await tick();
    const inputs = box?.querySelectorAll<HTMLInputElement>("input");
    inputs?.[inputs.length - 1]?.focus();
  }

  function onfocusout(e: FocusEvent) {
    const next = e.relatedTarget as Node | null;
    if (!next || !box?.contains(next)) oncommit();
  }
</script>

<div class="field">
  <span class="label" id={`${id}-label`}>
    {field.label}{#if field.required}<span class="muted" aria-hidden="true"> *</span>{/if}
  </span>
  <div
    class="rows"
    role="group"
    aria-labelledby={`${id}-label`}
    bind:this={box}
    {onfocusout}
  >
    {#each value as entry, i (i)}
      <div class="row">
        <input
          class="input"
          type="text"
          autocomplete="off"
          value={entry}
          aria-label={`${field.label}, entry ${i + 1}`}
          aria-invalid={error ? "true" : undefined}
          oninput={(e) => setRow(i, (e.currentTarget as HTMLInputElement).value)}
          onkeydown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              oncommit();
            }
          }}
        />
        <button
          type="button"
          class="btn subtle icon"
          aria-label={`Remove entry ${i + 1}`}
          title="Remove"
          onclick={() => removeRow(i)}
        >
          <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 4l8 8M12 4l-8 8" /></svg>
        </button>
      </div>
    {/each}
    <div>
      <button type="button" class="btn add" disabled={full} onclick={addRow}>
        <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 3v10M3 8h10" /></svg>
        Add
      </button>
    </div>
  </div>
  <span class="msg" aria-live="polite">
    {#if error}<span class="field-error">{error}</span>{:else if saved}<span class="field-saved">Saved</span>{/if}
  </span>
</div>

<style>
  .field {
    display: grid;
    gap: 4px;
  }

  .rows {
    display: grid;
    gap: 6px;
  }

  .row {
    display: flex;
    gap: 4px;
  }

  .msg:empty {
    display: none;
  }
</style>
