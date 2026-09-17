<script lang="ts">
  import type { Snippet } from "svelte";
  import "../app.css";

  let { children }: { children: Snippet } = $props();
</script>

<div class="app-shell">
	<aside class="sidebar">
		<div class="brand">
			<span class="brand-name">Gunda</span>
			<span class="brand-caption">Download manager</span>
		</div>

		<nav aria-label="Main navigation">
			<a class="new-link" href="/" aria-current="page">
			Downloads
		</a>
		</nav>

		<span class="development-label">Development preview</span>
	</aside>

	<main class="workspace">
		{@render children()}
	</main>
</div>