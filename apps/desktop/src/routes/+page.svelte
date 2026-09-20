<script lang="ts">
  import { onMount } from "svelte";
  import { Channel, invoke } from "@tauri-apps/api/core";

  import {
    formatBytes,
    percentage,
    type DownloadUpdate,
    type DownloadView,
    type ExecutionResponse,
  } from "$lib/downloads";

  let url = $state("");
  let directory = $state("");
  let downloads = $state<DownloadView[]>([]);

  let loading = $state(true);
  let submitting = $state(false);
  let choosing = $state(false);
  let cancelling = $state(false);
  let cancellationRequested = $state(false);

  let activeId = $state<string | null>(null);
  let error = $state("");
  let message = $state("");

  const busy = $derived(loading || submitting || choosing);

  onMount(() => {
    void loadDownloads();
  });

  function errorMessage(cause: unknown, fallback: string): string {
    return typeof cause === "string" ? cause : fallback;
  }

  function clearFeedback() {
    error = "";
    message = "";
  }

  function upsertDownload(job: DownloadView) {
    const exists = downloads.some((current) => current.id === job.id);

    downloads = exists
      ? downloads.map((current) => current.id === job.id ? job : current)
      : [job, ...downloads];
  }

  async function loadDownloads() {
    try {
      downloads = await invoke<DownloadView[]>("list_downloads");
    } catch (cause) {
      error = errorMessage(cause, "Could not load saved downloads.");
    } finally {
      loading = false;
    }
  }

  async function chooseDirectory() {
    if (busy) return;

    clearFeedback();
    choosing = true;

    try {
      const selected = await invoke<string | null>("choose_directory");

      if (selected !== null) {
        directory = selected;
      }
    } catch (cause) {
      error = errorMessage(cause, "Could not choose the destination folder.");
    } finally {
      choosing = false;
    }
  }

  async function startDownload(event: SubmitEvent) {
    event.preventDefault();

    if (busy) return;

    clearFeedback();

    if (!url.trim()) {
      error = "Enter a file URL.";
      return;
    }

    if (!directory) {
      error = "Choose where the file should be saved.";
      return;
    }

    submitting = true;
    cancellationRequested = false;

    let acceptingUpdates = true;
    const updates = new Channel<DownloadUpdate>();

    updates.onmessage = (update) => {
      if (!acceptingUpdates) return;

      if (update.type === "started") {
        activeId = update.job.id;
        upsertDownload(update.job);
        return;
      }

      downloads = downloads.map((job) =>
        job.id === update.id
          ? {
              ...job,
              written_bytes: update.written_bytes,
              total_bytes: update.total_bytes,
            }
          : job,
      );
    };

    try {
      const result = await invoke<ExecutionResponse>("start_download", {
        url,
        updates,
      });

      acceptingUpdates = false;
      upsertDownload(result.job);
      message = result.notice ?? "";
    } catch (cause) {
      error = errorMessage(cause, "Could not execute the download.");
    } finally {
      acceptingUpdates = false;
      submitting = false;
      activeId = null;
      cancelling = false;
      cancellationRequested = false;
    }
  }

  async function cancelDownload() {
    const id = activeId;

    if (id === null || cancelling || cancellationRequested) return;

    cancelling = true;

    try {
      const requested = await invoke<boolean>("cancel_download", { id });

      if (activeId === id) {
        cancellationRequested = requested;
      }
    } catch (cause) {
      error = errorMessage(cause, "Could not request cancellation.");
    } finally {
      cancelling = false;
    }
  }

  function statusLabel(job: DownloadView): string {
    if (job.id === activeId) {
      return cancellationRequested ? "Cancelling…" : "Running";
    }

    if (["inspecting", "downloading", "finalizing"].includes(job.state)) {
      return "Needs recovery";
    }

    return job.state.charAt(0).toUpperCase() + job.state.slice(1);
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

    <form onsubmit={startDownload}>
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
          {choosing ? "Opening…" : "Browse"}
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
          {submitting ? "Downloading…" : "Download"}
        </button>
      </div>
    </form>
  </section>

  <section class="downloads-list" aria-labelledby="downloads-heading">
  <div class="list-header">
    <h2 id="downloads-heading">Transfers</h2>
    <span>{downloads.length} items</span>
  </div>

  {#if loading}
    <div class="empty-state">
      <p>Loading downloads…</p>
    </div>
  {:else if downloads.length === 0}
    <div class="empty-state">
      <p>No downloads yet.</p>
      <span>New transfers will appear here.</span>
    </div>
  {:else}
    <div class="transfer-items">
      {#each downloads as job (job.id)}
        {@const percent = percentage(job)}

        <article class="transfer-item">
          <div class="transfer-heading">
            <strong>{job.name}</strong>
            <span>{statusLabel(job)}</span>
          </div>

          {#if percent !== null}
            <progress
              max="100"
              value={percent}
              aria-label={`Progress for ${job.name}`}
            ></progress>
          {:else if job.id === activeId}
            <progress
              aria-label={`Progress for ${job.name}`}
            ></progress>
          {/if}

          <div class="transfer-details">
            <span>
              {formatBytes(job.written_bytes)}
              {#if job.total_bytes !== null}
                / {formatBytes(job.total_bytes)}
              {/if}
            </span>

            {#if percent !== null}
              <span>{percent.toFixed(1)}%</span>
            {/if}
          </div>

          {#if job.error}
            <p class="error">{job.error}</p>
          {/if}

          {#if job.output_path}
            <p class="output-path">Output: {job.output_path}</p>
          {/if}

          {#if job.id === activeId}
            <button
              type="button"
              class="button secondary"
              onclick={cancelDownload}
              disabled={cancelling || cancellationRequested}
            >
              {cancellationRequested ? "Cancellation requested" : "Cancel"}
            </button>
          {/if}
        </article>
      {/each}
    </div>
  {/if}

  <p class="runtime-note">
    Keep Gunda open while a download is running.
  </p>
</section>
</div>