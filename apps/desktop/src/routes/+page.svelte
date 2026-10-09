<script lang="ts">
  import { onMount } from "svelte";
  import { Channel, invoke } from "@tauri-apps/api/core";

  import Icon from "$lib/Icon.svelte";
  import ThemeControl from "$lib/ThemeControl.svelte";
  import {
    formatAddedTime,
    formatBytes,
    percentage,
    stateLabel,
    totalBytes,
    type DownloadFilter,
    type DownloadUpdate,
    type DownloadView,
    type ExecutionResponse,
  } from "$lib/downloads";

  const filters: Array<{
    id: DownloadFilter;
    label: string;
    icon: "all" | "incomplete" | "completed";
  }> = [
    { id: "all", label: "All downloads", icon: "all" },
    { id: "incomplete", label: "Incomplete", icon: "incomplete" },
    { id: "completed", label: "Completed", icon: "completed" },
  ];

  let url = $state("");
  let directory = $state("");
  let downloads = $state<DownloadView[]>([]);
  let filter = $state<DownloadFilter>("all");
  let selectedId = $state<string | null>(null);
  let inspectorVisible = $state(true);

  let loading = $state(true);
  let submitting = $state(false);
  let choosing = $state(false);
  let cancelling = $state(false);
  let cancellationRequested = $state(false);

  let activeId = $state<string | null>(null);
  let error = $state("");
  let message = $state("");
  let dialogError = $state("");
  let transferError = $state("");
  let transferNotice = $state("");
  let transferId = $state<string | null>(null);

  let addButton: HTMLButtonElement;
  let addDialog: HTMLDialogElement;
  let urlInput: HTMLInputElement;
  let transferDialog: HTMLDialogElement;

  const busy = $derived(submitting || choosing);
  const selected = $derived(downloads.find((job) => job.id === selectedId) ?? null);
  const transfer = $derived(downloads.find((job) => job.id === transferId) ?? null);
  const transferPercent = $derived(transfer ? percentage(transfer) : null);
  const transferRunning = $derived(transfer !== null && transfer.id === activeId);
  const visibleDownloads = $derived(downloads.filter((job) => matchesFilter(job, filter)));
  const activeCount = $derived(downloads.filter(isActive).length);

  onMount(() => {
    void loadDownloads();
  });

  function errorMessage(cause: unknown, fallback: string): string {
    return typeof cause === "string" ? cause : fallback;
  }

  function isActive(job: DownloadView): boolean {
    return job.id === activeId || ["inspecting", "downloading", "finalizing"].includes(job.state);
  }

  function matchesFilter(job: DownloadView, value: DownloadFilter): boolean {
    return value === "all" || (value === "incomplete" ? job.state !== "completed" : job.state === value);
  }

  function countFor(value: DownloadFilter): number {
    return value === "all"
      ? downloads.length
      : value === "incomplete"
        ? downloads.filter((job) => job.state !== "completed").length
        : downloads.filter((job) => job.state === value).length;
  }

  function statusLabel(job: DownloadView): string {
    if (job.id === activeId) {
      return cancellationRequested ? "Cancelling" : "Downloading";
    }

    if (["inspecting", "downloading", "finalizing"].includes(job.state)) {
      return "Interrupted";
    }

    return stateLabel(job.state);
  }

  function clearFeedback() {
    error = "";
    message = "";
  }

  function completionNotice(result: ExecutionResponse): string {
    if (result.job.state === "cancelled") {
      return "Download cancelled.";
    }

    return result.notice ?? "";
  }

  function upsertDownload(job: DownloadView) {
    const exists = downloads.some((current) => current.id === job.id);
    downloads = exists
      ? downloads.map((current) => current.id === job.id ? job : current)
      : [job, ...downloads];
  }

  function selectDownload(id: string) {
    selectedId = id;
    inspectorVisible = true;
  }

  function inspectFromKeyboard(event: KeyboardEvent, id: string) {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      selectDownload(id);
    }
  }

  function openAddDialog() {
    clearFeedback();
    dialogError = "";
    addDialog.showModal();
    requestAnimationFrame(() => urlInput.focus());
  }

  function closeAddDialog() {
    if (!submitting) {
      addDialog.close();
    }
  }

  function returnAddFocus() {
    addButton?.focus();
  }

  function returnTransferFocus() {
    if (transferId) {
      document.querySelector<HTMLElement>(`[data-download-id="${transferId}"]`)?.focus();
    }
  }

  function openTransferDialog(id: string) {
    transferId = id;
    transferError = "";
    transferNotice = "";
    requestAnimationFrame(() => {
      if (!transferDialog.open) {
        transferDialog.showModal();
      }
    });
  }

  function hideTransferDialog() {
    if (transferDialog.open) {
      transferDialog.close();
    }
  }

  async function loadDownloads() {
    try {
      downloads = await invoke<DownloadView[]>("list_downloads");
      selectedId = downloads[0]?.id ?? null;
    } catch (cause) {
      error = errorMessage(cause, "Could not load saved downloads.");
    } finally {
      loading = false;
    }
  }

  async function chooseDirectory() {
    if (busy) return;

    dialogError = "";
    choosing = true;

    try {
      const selectedDirectory = await invoke<string | null>("choose_directory");
      if (selectedDirectory !== null) {
        directory = selectedDirectory;
      }
    } catch (cause) {
      dialogError = errorMessage(cause, "Could not choose the destination folder.");
    } finally {
      choosing = false;
    }
  }

  function createUpdatesChannel(): Channel<DownloadUpdate> {
    const updates = new Channel<DownloadUpdate>();

    updates.onmessage = (update) => {
      if (update.type === "started") {
        activeId = update.job.id;
        selectedId = update.job.id;
        upsertDownload(update.job);
        openTransferDialog(update.job.id);
        if (addDialog.open) {
          addDialog.close();
        }
        return;
      }

      downloads = downloads.map((job) =>
        job.id === update.id
          ? { ...job, written_bytes: update.written_bytes, total_bytes: update.total_bytes }
          : job,
      );
    };

    return updates;
  }

  async function startDownload(event: SubmitEvent) {
    event.preventDefault();
    if (busy) return;

    clearFeedback();
    dialogError = "";

    if (!url.trim()) {
      dialogError = "Enter a file URL.";
      urlInput.focus();
      return;
    }

    if (!directory) {
      dialogError = "Choose where the file should be saved.";
      return;
    }

    submitting = true;
    cancellationRequested = false;
    const updates = createUpdatesChannel();

    try {
      const result = await invoke<ExecutionResponse>("start_download", { url, updates });
      upsertDownload(result.job);
      message = completionNotice(result);
      transferNotice = completionNotice(result);
      url = "";
    } catch (cause) {
      const text = errorMessage(cause, "Couldn't start the download.");
      if (addDialog.open) {
        dialogError = text;
      } else {
        error = text;
        transferError = text;
      }
    } finally {
      submitting = false;
      activeId = null;
      cancelling = false;
      cancellationRequested = false;
    }
  }

  async function cancelDownload() {
    const id = activeId;
    if (id === null || cancelling || cancellationRequested) return;

    clearFeedback();
    cancelling = true;

    try {
      const requested = await invoke<boolean>("cancel_download", { id });
      if (activeId === id) {
        cancellationRequested = requested;
      }
    } catch (cause) {
      error = errorMessage(cause, "Could not cancel the download.");
      transferError = error;
    } finally {
      cancelling = false;
    }
  }

  async function resumeSelected() {
    const job = selected;
    if (job === null || !job.can_resume || busy) return;

    clearFeedback();
    submitting = true;
    cancellationRequested = false;
    const updates = createUpdatesChannel();

    try {
      const result = await invoke<ExecutionResponse>("resume_download", {
        id: job.id,
        updates,
      });
      upsertDownload(result.job);
      message = completionNotice(result);
      transferNotice = completionNotice(result);
    } catch (cause) {
      error = errorMessage(cause, "Couldn't resume the download.");
      transferError = error;
    } finally {
      submitting = false;
      activeId = null;
      cancelling = false;
      cancellationRequested = false;
    }
  }
</script>

<svelte:head>
  <title>Downloads — Gunda</title>
</svelte:head>

<div class="app-shell">
  <aside class="sidebar" aria-label="Download filters">
    <div class="sidebar-cap" aria-hidden="true"></div>

    <nav class="filter-list" aria-label="Filter downloads">
      {#each filters as item}
        <button
          type="button"
          class:current={filter === item.id}
          aria-pressed={filter === item.id}
          onclick={() => filter = item.id}
        >
          <Icon name={item.icon} size={15} />
          <span>{item.label}</span>
          <span class="filter-count">{countFor(item.id)}</span>
        </button>
      {/each}
    </nav>

    <div class="sidebar-footer">
      <ThemeControl />
    </div>
  </aside>

  <main class="workspace">
    <header class="toolbar" aria-label="Download actions">
      <div class="toolbar-primary">
        <button bind:this={addButton} type="button" class="toolbar-button accent" onclick={openAddDialog} disabled={busy}>
          <Icon name="add" />
          <span>Add download</span>
        </button>
        <span class="toolbar-divider"></span>
        <button type="button" class="toolbar-button" onclick={resumeSelected} disabled={!selected?.can_resume || busy}>
          <Icon name="resume" />
          <span>Resume</span>
        </button>
        <button type="button" class="toolbar-button" onclick={cancelDownload} disabled={activeId === null || cancelling || cancellationRequested}>
          <Icon name="cancel" />
          <span>{cancellationRequested ? "Cancelling" : "Cancel"}</span>
        </button>
        <span class="toolbar-divider"></span>
        <button type="button" class="toolbar-button" onclick={() => inspectorVisible = true} disabled={selected === null || inspectorVisible}>
          <Icon name="details" />
          <span>Inspect</span>
        </button>
      </div>
      <span class="toolbar-summary">{downloads.length} downloads</span>
    </header>

    {#if error || message}
      <div class:error={error} class:notice={!error} class="app-feedback" role={error ? "alert" : "status"}>
        <span>{error || message}</span>
        <button type="button" aria-label="Dismiss message" onclick={clearFeedback}><Icon name="close" size={14} /></button>
      </div>
    {/if}

    <div class="content-layout" class:without-inspector={!selected || !inspectorVisible}>
      <section class="downloads-panel" aria-labelledby="downloads-heading">
        <div class="panel-heading">
          <div>
            <h1 id="downloads-heading">{filters.find((item) => item.id === filter)?.label}</h1>
            <p>{visibleDownloads.length} {visibleDownloads.length === 1 ? "item" : "items"}</p>
          </div>
        </div>

        {#if loading}
          <div class="empty-state" aria-live="polite">
            <Icon name="download" size={22} />
            <strong>Loading downloads…</strong>
          </div>
        {:else if downloads.length === 0}
          <div class="empty-state">
            <Icon name="download" size={24} />
            <strong>No downloads yet</strong>
            <span>Add a URL to start your first transfer.</span>
            <button type="button" class="button primary" onclick={openAddDialog}>Add download</button>
          </div>
        {:else if visibleDownloads.length === 0}
          <div class="empty-state">
            <Icon name="all" size={22} />
            <strong>No {filters.find((item) => item.id === filter)?.label.toLowerCase()}</strong>
            <span>Choose another filter to see your downloads.</span>
          </div>
        {:else}
          <div class="table-scroll">
            <table class="downloads-table">
              <thead>
                <tr>
                  <th scope="col">Name</th>
                  <th scope="col">Status</th>
                  <th scope="col">Progress</th>
                  <th scope="col">Source</th>
                  <th scope="col">Added</th>
                </tr>
              </thead>
              <tbody>
                {#each visibleDownloads as job (job.id)}
                  {@const percent = percentage(job)}
                  {@const total = totalBytes(job)}
                  <tr
                    class:selected={selectedId === job.id}
                    class:completed={job.state === "completed"}
                    data-download-id={job.id}
                    tabindex="0"
                    aria-selected={selectedId === job.id}
                    onclick={() => selectDownload(job.id)}
                    onkeydown={(event) => inspectFromKeyboard(event, job.id)}
                  >
                    <td class="name-cell" title={job.name}>
                      <span class="file-icon"><Icon name="download" size={14} /></span>
                      <span title={job.name}>{job.name}</span>
                    </td>
                    <td><span class={`status status-${job.id === activeId ? "active" : job.state}`}><span></span>{statusLabel(job)}</span></td>
                    <td class="progress-cell">
                      <div class="size-line">
                        <span>{#if total}{formatBytes(job.written_bytes)} / {formatBytes(total)}{:else}{formatBytes(job.written_bytes)} downloaded{/if}</span>
                        {#if percent !== null}<span>{percent.toFixed(1)}%</span>{/if}
                      </div>
                      {#if percent !== null}
                        <div class="progress-track" role="progressbar" aria-label={`Progress for ${job.name}`} aria-valuemin="0" aria-valuemax="100" aria-valuenow={percent}>
                          <span style={`width: ${percent}%`}></span>
                        </div>
                      {/if}
                    </td>
                    <td class="source-cell" title={job.source}>{job.source}</td>
                    <td class="added-cell" title={formatAddedTime(job.created_at)}>{formatAddedTime(job.created_at)}</td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
        {/if}
      </section>

      {#if selected && inspectorVisible}
        {@const selectedPercent = percentage(selected)}
        {@const selectedTotal = totalBytes(selected)}
        <aside class="inspector" aria-labelledby="inspector-heading">
          <header class="inspector-header">
            <div>
              <h2 id="inspector-heading">Details</h2>
              <span class="inspector-name">{selected.name}</span>
            </div>
            <button type="button" class="icon-button" aria-label="Close details" onclick={() => inspectorVisible = false}><Icon name="close" /></button>
          </header>

          <div class="inspector-body">
            <div class="inspector-summary">
              <span class={`status status-${selected.id === activeId ? "active" : selected.state}`}><span></span>{statusLabel(selected)}</span>
              <span>{#if selectedTotal}{formatBytes(selected.written_bytes)} of {formatBytes(selectedTotal)}{:else}{formatBytes(selected.written_bytes)} downloaded{/if}</span>
              {#if selectedPercent !== null}<span>{selectedPercent.toFixed(1)}%</span>{/if}
            </div>

            {#if selectedPercent !== null}
              <div class="progress-track large" role="progressbar" aria-label={`Progress for ${selected.name}`} aria-valuemin="0" aria-valuemax="100" aria-valuenow={selectedPercent}>
                <span style={`width: ${selectedPercent}%`}></span>
              </div>
            {/if}

            <dl class="metadata-grid">
              <div><dt>File name</dt><dd title={selected.name}>{selected.name}</dd></div>
              <div><dt>Source</dt><dd title={selected.source}>{selected.source}</dd></div>
              <div><dt>Added</dt><dd title={formatAddedTime(selected.created_at)}>{formatAddedTime(selected.created_at)}</dd></div>
              <div><dt>Added via</dt><dd title={selected.added_via}>{selected.added_via}</dd></div>
              {#if selected.content_type}<div><dt>Content type</dt><dd title={selected.content_type}>{selected.content_type}</dd></div>{/if}
              {#if selected.output_path}<div class="wide"><dt>Saved to</dt><dd title={selected.output_path}>{selected.output_path}</dd></div>{/if}
            </dl>

            {#if selected.error}
              <div class="failure-details" role="alert">
                <strong>Why it stopped</strong>
                <p>{selected.error}</p>
              </div>
            {/if}
          </div>
        </aside>
      {/if}
    </div>

    <footer class="status-bar">
      <span>{downloads.length} downloads ({activeCount} active)</span>
      <span>{activeId ? "Transfer in progress" : "Ready"}</span>
    </footer>
  </main>
</div>

<dialog bind:this={addDialog} class="download-dialog" onclose={returnAddFocus} oncancel={(event) => submitting && event.preventDefault()}>
  <form onsubmit={startDownload}>
    <header class="dialog-header">
      <h2>Add download</h2>
      <button type="button" class="icon-button" aria-label="Close add download dialog" onclick={closeAddDialog} disabled={submitting}><Icon name="close" /></button>
    </header>

    <div class="dialog-body">
      <label for="download-url">File URL</label>
      <input bind:this={urlInput} id="download-url" type="url" inputmode="url" autocomplete="off" spellcheck="false" placeholder="https://example.com/file.zip" bind:value={url} oninput={() => dialogError = ""} disabled={busy} />

      <label for="download-directory">Destination folder</label>
      <div class="directory-row">
        <input id="download-directory" type="text" value={directory} placeholder="Choose a folder" readonly aria-describedby="destination-help" />
        <button type="button" class="button secondary" onclick={chooseDirectory} disabled={busy}><Icon name="folder" size={15} />{choosing ? "Opening…" : "Browse"}</button>
      </div>
      <p id="destination-help" class="field-help">Gunda will choose a safe file name and will not overwrite an existing file.</p>

      <div class="dialog-feedback" aria-live="polite">
        {#if dialogError}<p class="error">{dialogError}</p>{/if}
      </div>
    </div>

    <footer class="dialog-footer">
      <button type="button" class="button secondary" onclick={closeAddDialog} disabled={submitting}>Cancel</button>
      <button type="submit" class="button primary" disabled={busy}>{submitting ? "Starting…" : "Download"}</button>
    </footer>
  </form>
</dialog>

<dialog
  bind:this={transferDialog}
  class="download-dialog transfer-dialog"
  aria-labelledby="transfer-heading"
  onclose={returnTransferFocus}
  oncancel={() => {}}
>
  {#if transfer}
    {@const transferTotal = totalBytes(transfer)}
    <header class="dialog-header">
      <div class="transfer-title">
        <h2 id="transfer-heading">{transferRunning ? "Downloading" : stateLabel(transfer.state)}</h2>
        <span title={transfer.name}>{transfer.name}</span>
      </div>
      <button type="button" class="icon-button" aria-label="Hide transfer window" onclick={hideTransferDialog}><Icon name="close" /></button>
    </header>

    <div class="transfer-dialog-body">
      <div class="transfer-metrics">
        <span>{#if transferTotal}{formatBytes(transfer.written_bytes)} of {formatBytes(transferTotal)}{:else}{formatBytes(transfer.written_bytes)} downloaded{/if}</span>
        {#if transferPercent !== null}<strong>{transferPercent.toFixed(1)}%</strong>{/if}
      </div>
      {#if transferPercent !== null}
        <div class="progress-track large" role="progressbar" aria-label={`Progress for ${transfer.name}`} aria-valuemin="0" aria-valuemax="100" aria-valuenow={transferPercent}>
          <span style={`width: ${transferPercent}%`}></span>
        </div>
      {:else if transferRunning}
        <p class="unknown-progress" role="status">The total size will be shown when the download finishes.</p>
      {/if}

      {#if transferError}
        <div class="failure-details" role="alert"><strong>Could not continue</strong><p>{transferError}</p></div>
      {:else if transfer.error}
        <div class="failure-details" role="alert"><strong>Download failed</strong><p>{transfer.error}</p></div>
      {:else if transferNotice}
        <p class="transfer-notice" role="status">{transferNotice}</p>
      {:else if !transferRunning}
        <p class="transfer-notice" role="status">{stateLabel(transfer.state)}.</p>
      {/if}
    </div>

    <footer class="dialog-footer">
      {#if transferRunning}
        <button type="button" class="button secondary" onclick={hideTransferDialog}>Hide</button>
        <button type="button" class="button primary danger-button" onclick={cancelDownload} disabled={cancelling || cancellationRequested}>
          {cancellationRequested || cancelling ? "Cancelling…" : "Cancel download"}
        </button>
      {:else}
        <button type="button" class="button primary" onclick={hideTransferDialog}>Done</button>
      {/if}
    </footer>
  {/if}
</dialog>
