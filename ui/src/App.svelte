<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { emit, listen } from "@tauri-apps/api/event";
  import { onMount } from "svelte";

  let version = $state("");
  let focused = $state<string | null>(null);

  onMount(() => {
    let stop: (() => void) | undefined;
    let gone = false;
    invoke<string>("get_version").then(
      (v) => (version = v),
      () => (version = ""),
    );
    listen<string>("perchbar://focus", (e) => (focused = e.payload)).then((un) => {
      if (gone) {
        un();
        return;
      }
      stop = un;
      // Only now can a focus request be heard.
      void emit("perchbar://ready");
    });
    return () => {
      gone = true;
      stop?.();
    };
  });
</script>

<header class="titlebar">
  <h1>Perchbar</h1>
  {#if version}<span class="version">{version}</span>{/if}
</header>

<main>
  {#if focused}
    <p>Settings for {focused} will show here.</p>
  {:else}
    <p>Settings will show here.</p>
  {/if}
</main>

<style>
  .titlebar {
    display: flex;
    align-items: baseline;
    gap: 8px;
    padding: 16px 24px 12px;
    border-bottom: 1px solid var(--divider);
  }

  h1 {
    margin: 0;
    font-size: 20px;
    font-weight: 600;
  }

  .version {
    color: var(--text-muted);
    font-size: 12px;
  }

  main {
    padding: 16px 24px;
    color: var(--text-muted);
  }
</style>
