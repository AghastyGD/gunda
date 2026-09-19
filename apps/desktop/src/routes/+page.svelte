<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";

  let url = $state("");
  let directory = $state("");
  let busy = $state(false);
  let error = $state("");
  let message = $state("");

  function clearFeedback() {
    error = "";
    message = "";
  }

  async function chooseDirectory() {
    clearFeedback();

    try {
      const selected = await invoke<string | null>("choose_directory");

      if (selected !== null) {
        directory = selected;
      }
    } catch (cause) {
      error =
        typeof cause === "string"
          ? cause
          : "Could not choose the destination folder.";
    }
  }

  async function validateInput(event: SubmitEvent) {
    event.preventDefault();
    clearFeedback();

    if (!url.trim()) {
      error = "Enter a URL.";
      return;
    }

    if (!directory) {
      error = "Choose where the file should be saved.";
      return;
    }

    busy = true;

    try {
      await invoke("validate_download_input", { url });

      message = "URL looks good. Download execution is not connected yet.";
    } catch (cause) {
      error =
        typeof cause === "string"
          ? cause
          : "Could not validate the URL.";
    } finally {
      busy = false;
    }
  }
</script>

<svelte:head>
  <title>Downloads — Gunda</title>
</svelte:head>

<div class="downloads-page">
  <header class="page-header">
    <h1>Downloads</h1>
  </header>

  <section class="new-download" aria-labelledby="new-download-heading">
    <div class="section-heading">
      <h2 id="new-download-heading">New download</h2>
    </div>

    <form onsubmit={validateInput}>
      <div class="field">
        <label for="download-url">URL</label>

        <input
          id="download-url"
          type="text"
          inputmode="url"
          autocomplete="off"
          spellcheck="false"
          placeholder="https://example.com/file.zip"
          bind:value={url}
          oninput={clearFeedback}
          disabled={busy}
        />
      </div>

      <div class="field">
        <label for="download-directory">Destination</label>

        <div class="directory-row">
          <input
            id="download-directory"
            type="text"
            value={directory}
            placeholder="Choose a folder"
            readonly
          />

          <button
            type="button"
            class="button secondary"
            onclick={chooseDirectory}
            disabled={busy}
          >
            Browse
          </button>
        </div>
      </div>

      <div class="form-footer">
        <div class="feedback" aria-live="polite">
          {#if error}
            <p class="error">{error}</p>
          {:else if message}
            <p class="success">{message}</p>
          {/if}
        </div>

        <button class="button primary" type="submit" disabled={busy}>
          {busy ? "Checking…" : "Check URL"}
        </button>
      </div>
    </form>
  </section>

  <section class="downloads-list" aria-labelledby="downloads-heading">
    <div class="list-header">
      <h2 id="downloads-heading">Transfers</h2>
      <span>0 items</span>
    </div>

    <div class="empty-state">
      <p>No downloads yet.</p>
      <span>New transfers will appear here.</span>
    </div>
  </section>
</div>