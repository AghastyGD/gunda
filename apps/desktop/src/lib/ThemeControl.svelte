<script lang="ts">
  import { onMount } from "svelte";
  import Icon from "$lib/Icon.svelte";

  type Theme = "system" | "light" | "dark";

  let theme = $state<Theme>("system");

  onMount(() => {
    const saved = localStorage.getItem("gunda-theme");
    if (saved === "light" || saved === "dark" || saved === "system") {
      theme = saved;
    }
    applyTheme(theme);
  });

  function applyTheme(value: Theme) {
    document.documentElement.dataset.theme = value;
  }

  function changeTheme(event: Event) {
    theme = (event.currentTarget as HTMLSelectElement).value as Theme;
    localStorage.setItem("gunda-theme", theme);
    applyTheme(theme);
  }
</script>

<label class="theme-control">
  <Icon name="theme" size={15} />
  <span class="visually-hidden">Colour theme</span>
  <select value={theme} onchange={changeTheme} aria-label="Colour theme">
    <option value="system">System</option>
    <option value="light">Light</option>
    <option value="dark">Dark</option>
  </select>
</label>
