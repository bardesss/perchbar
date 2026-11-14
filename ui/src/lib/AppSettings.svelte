<script lang="ts">
  // The "Perchbar" section: Start with Windows, version, log folder.
  import { openLogFolder, setAutostart, type CommandError } from "../api";

  interface Props {
    autostart: boolean;
    version: string;
    onchanged: () => Promise<void>;
  }

  let { autostart, version, onchanged }: Props = $props();

  let error: string | null = $state(null);
  let busy = $state(false);

  async function toggle(e: Event) {
    const box = e.currentTarget as HTMLInputElement;
    // Not disabled while busy: that would drop keyboard focus.
    if (busy) {
      box.checked = !box.checked;
      return;
    }
    busy = true;
    error = null;
    try {
      await setAutostart(box.checked);
    } catch (err) {
      error = (err as CommandError).message;
    }
    // Show what Windows has now, whichever way it went.
    await onchanged();
    box.checked = autostart;
    busy = false;
  }

  async function openFolder() {
    error = null;
    try {
      await openLogFolder();
    } catch (err) {
      error = (err as CommandError).message;
    }
  }
</script>

<section class="app" aria-labelledby="app-title">
  <h2 id="app-title">Perchbar</h2>
  <div class="row">
    <label for="autostart">Start with Windows</label>
    <input
      id="autostart"
      class="switch"
      type="checkbox"
      role="switch"
      checked={autostart}
      aria-busy={busy}
      onchange={toggle}
    />
  </div>
  {#if error}<p class="field-error" role="alert">{error}</p>{/if}
  <div class="row">
    <span class="muted">{version ? `Version ${version}` : ""}</span>
    <button type="button" class="btn subtle" onclick={openFolder}>Open log folder</button>
  </div>
</section>

<style>
  .app {
    display: grid;
    gap: 4px;
    padding: 12px;
    border-top: 1px solid var(--divider);
  }

  h2 {
    margin: 0 0 4px;
    font-size: 14px;
    line-height: 20px;
    font-weight: 600;
  }

  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    min-height: 32px;
  }

  p {
    margin: 0;
  }
</style>
