<script lang="ts">
  // What a widget may reach, with a button to approve it (Install or Allow)
  // and, optionally, Cancel. Everything listed comes from a widget folder,
  // so it goes through text bindings only.
  import type { Permissions } from "../api";
  import { permissionLines } from "./permissions";

  interface Props {
    permissions: Permissions;
    /** Shown above the list, e.g. what changed. */
    intro?: string;
    /** A warning shown before everything else. */
    notice?: string;
    confirmLabel: string;
    busy?: boolean;
    error?: string | null;
    onconfirm: () => void;
    oncancel?: () => void;
  }

  let { permissions, intro, notice, confirmLabel, busy = false, error = null, onconfirm, oncancel }: Props = $props();

  const lines = $derived(permissionLines(permissions));
</script>

<section class="review" aria-label="Permissions">
  {#if notice}<p class="notice" role="note">{notice}</p>{/if}
  {#if intro}<p class="intro">{intro}</p>{/if}
  {#each lines as line (line.lead)}
    <div class="line">
      <p class="lead">{line.lead}</p>
      {#if line.items.length > 0}
        <ul>
          {#each line.items as item, i (i)}
            <li>{item}</li>
          {/each}
        </ul>
      {/if}
    </div>
  {/each}
  {#if error}<p class="field-error" role="alert">{error}</p>{/if}
  <div class="buttons">
    <button type="button" class="btn accent" disabled={busy} onclick={onconfirm}>{confirmLabel}</button>
    {#if oncancel}
      <button type="button" class="btn" disabled={busy} onclick={oncancel}>Cancel</button>
    {/if}
  </div>
</section>

<style>
  .review {
    display: grid;
    gap: 8px;
  }

  p {
    margin: 0;
  }

  .notice {
    padding: 8px 12px;
    border: 1px solid var(--danger);
    border-radius: var(--radius);
  }

  .lead {
    font-weight: 600;
  }

  ul {
    margin: 2px 0 0;
    padding-left: 20px;
  }

  li {
    overflow-wrap: anywhere;
    font-family: "Cascadia Mono", Consolas, monospace;
    font-size: 12px;
    line-height: 18px;
  }

  .buttons {
    display: flex;
    gap: 8px;
    margin-top: 4px;
  }
</style>
