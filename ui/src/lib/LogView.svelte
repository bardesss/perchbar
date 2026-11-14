<script lang="ts">
  // The instance's recent log lines, fetched while the section is open and
  // again on every state change (`changes` goes up).
  import { getLogs, type CommandError, type LogLine } from "../api";

  interface Props {
    instanceId: string;
    changes: number;
  }

  let { instanceId, changes }: Props = $props();

  let open = $state(false);
  let lines: LogLine[] = $state([]);
  let error: string | null = $state(null);
  let seq = 0;

  $effect(() => {
    if (!open) return;
    void changes;
    const mine = ++seq;
    getLogs(instanceId).then(
      (l) => {
        if (mine !== seq) return;
        lines = l;
        error = null;
      },
      (e) => {
        if (mine === seq) error = (e as CommandError).message;
      },
    );
  });

  function time(at: string): string {
    const d = new Date(at);
    return Number.isNaN(d.getTime()) ? at : d.toLocaleTimeString();
  }
</script>

<details bind:open>
  <summary>Log</summary>
  {#if error}
    <p class="field-error">{error}</p>
  {:else if lines.length === 0}
    <p class="muted">Nothing logged yet.</p>
  {:else}
    <ol class="lines">
      {#each lines as line, i (i)}
        <li class={line.level}>
          <time datetime={line.at}>{time(line.at)}</time>
          <span class="level">{line.level}</span>
          <span class="text">{line.text}</span>
        </li>
      {/each}
    </ol>
  {/if}
</details>

<style>
  details {
    border: 1px solid var(--card-stroke);
    border-radius: var(--radius);
    background: var(--card);
  }

  summary {
    padding: 8px 12px;
    cursor: default;
    border-radius: var(--radius);
  }

  summary:hover {
    background: var(--subtle-hover);
  }

  p {
    margin: 0;
    padding: 0 12px 12px;
  }

  .lines {
    list-style: none;
    margin: 0;
    padding: 0 12px 12px;
    max-height: 240px;
    overflow: auto;
    font-family: "Cascadia Mono", Consolas, monospace;
    font-size: 12px;
    line-height: 18px;
  }

  li {
    display: grid;
    grid-template-columns: auto 3.5em 1fr;
    gap: 8px;
  }

  time,
  .level {
    color: var(--text-muted);
  }

  .text {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .warn .level {
    color: var(--text);
  }

  .error .level,
  .error .text {
    color: var(--danger);
  }
</style>
