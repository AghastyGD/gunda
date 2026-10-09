<script lang="ts">
  type IconName =
    | "add"
    | "all"
    | "active"
    | "cancel"
    | "cancelled"
    | "chevron"
    | "close"
    | "completed"
    | "details"
    | "download"
    | "failed"
    | "folder"
    | "incomplete"
    | "interrupted"
    | "resume"
    | "theme";

  let { name, size = 16 }: { name: IconName; size?: number } = $props();
</script>

<svg
  class="icon"
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width="1.8"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
>
  {#if name === "add"}
    <path d="M12 5v14M5 12h14" />
  {:else if name === "download"}
    <path d="M12 3v12m0 0 4-4m-4 4-4-4M5 20h14" />
  {:else if name === "all"}
    <path d="M4 5.5h16v13H4zM8 9h8M8 13h6" />
  {:else if name === "active"}
    <path d="M12 3a9 9 0 1 0 9 9" /><path d="M16 3h5v5" />
  {:else if name === "completed"}
    <circle cx="12" cy="12" r="9" /><path d="m8 12 2.5 2.5L16 9" />
  {:else if name === "incomplete"}
    <circle cx="12" cy="12" r="9" /><path d="M12 7v5l3 2" />
  {:else if name === "failed"}
    <circle cx="12" cy="12" r="9" /><path d="m9 9 6 6m0-6-6 6" />
  {:else if name === "cancelled"}
    <circle cx="12" cy="12" r="9" /><path d="M8 12h8" />
  {:else if name === "interrupted"}
    <path d="M12 3 2.8 19h18.4z" /><path d="M12 9v4m0 3h.01" />
  {:else if name === "resume"}
    <path d="m9 6 8 6-8 6z" />
  {:else if name === "cancel" || name === "close"}
    <path d="m6 6 12 12M18 6 6 18" />
  {:else if name === "details"}
    <circle cx="12" cy="12" r="9" /><path d="M12 11v5m0-8h.01" />
  {:else if name === "folder"}
    <path d="M3 6.5h7l2 2h9v10H3z" />
  {:else if name === "theme"}
    <circle cx="12" cy="12" r="4" /><path d="M12 2v2m0 16v2M2 12h2m16 0h2M5 5l1.5 1.5m11 11L19 19M19 5l-1.5 1.5m-11 11L5 19" />
  {:else if name === "chevron"}
    <path d="m8 10 4 4 4-4" />
  {/if}
</svg>
