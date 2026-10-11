import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import Page from "./+page.svelte";
import type { DownloadUpdate, DownloadView, ExecutionResponse } from "$lib/downloads";

const tauri = vi.hoisted(() => ({ invoke: vi.fn(), channels: [] as Array<{ onmessage: ((m: DownloadUpdate) => void) | null }> }));
vi.mock("@tauri-apps/api/core", () => ({
  invoke: tauri.invoke,
  Channel: class {
    onmessage: ((m: DownloadUpdate) => void) | null = null;
    constructor() { tauri.channels.push(this); }
  },
}));

function job(overrides: Partial<DownloadView> = {}): DownloadView {
  return {
    id: "job-1", name: "archive.zip", state: "failed", written_bytes: "4", total_bytes: "8",
    source: "https://example.test/archive.zip", added_via: "Desktop", created_at: "1791500000",
    content_type: "application/zip", output_path: "/downloads/archive.zip",
    error: "The server ended the connection.", can_resume: false, ...overrides,
  };
}

function emit(update: DownloadUpdate) {
  const channel = tauri.channels.at(-1);
  if (!channel?.onmessage) throw new Error("Expected a Tauri update channel");
  channel.onmessage(update);
}

beforeEach(() => {
  tauri.channels = [];
  tauri.invoke.mockReset();
  tauri.invoke.mockImplementation(async (command: string) => {
    if (command === "list_downloads") return [];
    if (command === "choose_directory") return "/tmp/gunda-test-output";
    if (command === "cancel_download") return true;
    throw new Error(`Unexpected command: ${command}`);
  });
  localStorage.clear();
});
afterEach(() => cleanup());

describe("downloads page", () => {
  it("shows filter counts, supports keyboard selection, and presents safe details", async () => {
    tauri.invoke.mockResolvedValueOnce([
      job(),
      job({ id: "job-2", name: "done.pdf", state: "completed", written_bytes: "64", total_bytes: null, error: null }),
      job({ id: "job-3", name: "stopped.iso", state: "interrupted", written_bytes: "2", can_resume: true }),
    ]);
    render(Page);
    await screen.findAllByText("archive.zip");
    expect(screen.getByRole("button", { name: /All downloads/ }).textContent).toContain("3");
    expect(screen.getByRole("button", { name: /Incomplete/ }).textContent).toContain("2");
    expect(screen.getByRole("button", { name: /Completed/ }).textContent).toContain("1");
    expect(screen.getByRole("alert").textContent).toContain("The server ended the connection.");
    expect(document.querySelector('[data-download-id="job-2"]')?.textContent).toContain("64 B / 64 B");
    expect(screen.queryByText("/downloads/archive.zip")).toBeTruthy();
    expect(screen.queryByText("authorization", { exact: false })).toBeNull();
    const row = document.querySelector('[data-download-id="job-3"]')!;
    await fireEvent.keyDown(row, { key: "Enter" });
    expect(row.getAttribute("aria-selected")).toBe("true");
    expect(screen.getByText("Details").parentElement?.textContent).toContain("stopped.iso");
    await fireEvent.click(screen.getByRole("button", { name: /Completed/ }));
    expect(screen.getByText("done.pdf")).toBeTruthy();
    expect(screen.queryByText("archive.zip")).toBeNull();
  });

  it("uses Browse, submits a URL, and reflects started, progress, and final updates", async () => {
    let finish!: (value: ExecutionResponse) => void;
    tauri.invoke.mockImplementation((command: string) => {
      if (command === "list_downloads") return Promise.resolve([]);
      if (command === "choose_directory") return Promise.resolve("/tmp/gunda-test-output");
      if (command === "start_download") return new Promise((resolve) => { finish = resolve; });
      throw new Error(`Unexpected command: ${command}`);
    });
    render(Page);
    await screen.findByText("No downloads yet");
    await fireEvent.click(within(screen.getByRole("banner", { name: "Download actions" })).getByRole("button", { name: "Add download" }));
    await fireEvent.click(screen.getByRole("button", { name: "Browse" }));
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("choose_directory"));
    await fireEvent.input(screen.getByLabelText("File URL"), { target: { value: "https://example.test/archive.zip" } });
    await fireEvent.click(screen.getByRole("button", { name: "Download" }));
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("start_download", expect.objectContaining({ url: "https://example.test/archive.zip", updates: expect.any(Object) })));
    const started = job({ id: "live", name: "live.bin", state: "downloading", written_bytes: "0", total_bytes: null, error: null });
    emit({ type: "started", job: started });
    await screen.findByRole("heading", { name: "Downloading" });
    emit({ type: "progress", id: "live", written_bytes: "512", total_bytes: "1024" });
    await waitFor(() => expect(document.querySelector('[data-download-id="live"]')?.textContent).toContain("512 B / 1.0 KiB"));
    expect(document.querySelector('[role="progressbar"][aria-label="Progress for live.bin"]')?.getAttribute("aria-valuenow")).toBe("50");
    finish({ job: job({ id: "live", name: "live.bin", state: "completed", written_bytes: "1024", total_bytes: null, error: null }), notice: null });
    await waitFor(() => expect(document.querySelector('[data-download-id="live"]')?.textContent).toContain("Completed"));
  });

  it("keeps transfers running when hidden and reports cancellation feedback", async () => {
    let finish!: (value: ExecutionResponse) => void;
    tauri.invoke.mockImplementation((command: string) => {
      if (command === "list_downloads") return Promise.resolve([]);
      if (command === "choose_directory") return Promise.resolve("/tmp/gunda-test-output");
      if (command === "start_download") return new Promise((resolve) => { finish = resolve; });
      if (command === "cancel_download") return Promise.resolve(true);
      throw new Error(`Unexpected command: ${command}`);
    });
    render(Page);
    await screen.findByText("No downloads yet");
    await fireEvent.click(within(screen.getByRole("banner", { name: "Download actions" })).getByRole("button", { name: "Add download" }));
    await fireEvent.click(screen.getByRole("button", { name: "Browse" }));
    await waitFor(() => expect((screen.getByLabelText("Destination folder") as HTMLInputElement).value).toBe("/tmp/gunda-test-output"));
    await fireEvent.input(screen.getByLabelText("File URL"), { target: { value: "https://example.test/live.bin" } });
    await fireEvent.click(screen.getByRole("button", { name: "Download" }));
    emit({ type: "started", job: job({ id: "live", name: "live.bin", state: "downloading", total_bytes: "1000", error: null }) });
    await screen.findByRole("heading", { name: "Downloading" });
    await fireEvent.click(screen.getByRole("button", { name: "Hide transfer window" }));
    expect(screen.queryByRole("heading", { name: "Downloading" })).toBeNull();
    expect(tauri.invoke).not.toHaveBeenCalledWith("cancel_download", expect.anything());
    emit({ type: "progress", id: "live", written_bytes: "250", total_bytes: "1000" });
    await waitFor(() => expect(document.querySelector('[data-download-id="live"]')?.textContent).toContain("250 B / 1000 B"));
    await fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("cancel_download", { id: "live" }));
    await waitFor(() => expect(document.querySelector('[data-download-id="live"]')?.textContent).toContain("Cancelling"));
    finish({ job: job({ id: "live", name: "live.bin", state: "cancelled", written_bytes: "250", total_bytes: "1000", error: null }), notice: null });
    await waitFor(() => expect(document.querySelector('[role="status"]')?.textContent).toContain("Download cancelled."));
  });

  it("shows list and Browse command errors", async () => {
    tauri.invoke.mockRejectedValueOnce("The request failed.");
    render(Page);
    expect((await screen.findByRole("alert")).textContent).toContain("The request failed.");
    await fireEvent.click(within(screen.getByRole("banner", { name: "Download actions" })).getByRole("button", { name: "Add download" }));
    tauri.invoke.mockRejectedValueOnce("Folder access was denied.");
    await fireEvent.click(screen.getByRole("button", { name: "Browse" }));
    expect(await screen.findByText("Folder access was denied.")).toBeTruthy();
  });

  it("restores focus after Escape closes the add dialog", async () => {
    render(Page);
    await screen.findByText("No downloads yet");
    const add = within(screen.getByRole("banner", { name: "Download actions" })).getByRole("button", { name: "Add download" });
    await fireEvent.click(add);
    const dialog = screen.getByRole("dialog", { name: "Add download" }) as HTMLDialogElement;
    await waitFor(() => expect(document.activeElement).toBe(screen.getByLabelText("File URL")));
    const escape = new Event("cancel", { cancelable: true });
    dialog.dispatchEvent(escape);
    expect(escape.defaultPrevented).toBe(false);
    dialog.close();
    await waitFor(() => expect(document.activeElement).toBe(add));
  });

  it("resumes only backend-eligible jobs and reports start/cancel failures", async () => {
    let finishResume!: (value: ExecutionResponse) => void;
    tauri.invoke.mockImplementation((command: string) => {
      if (command === "list_downloads") return Promise.resolve([job({ state: "interrupted", can_resume: true })]);
      if (command === "resume_download") return new Promise((resolve) => { finishResume = resolve; });
      if (command === "start_download") return Promise.reject("The file could not be started.");
      if (command === "choose_directory") return Promise.resolve("/tmp/gunda-test-output");
      if (command === "cancel_download") return Promise.reject("The download could not be cancelled.");
      throw new Error(`Unexpected command: ${command}`);
    });
    render(Page);
    await waitFor(() => expect(document.querySelector('[data-download-id="job-1"]')).not.toBeNull());
    await fireEvent.click(screen.getByRole("button", { name: "Resume" }));
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("resume_download", expect.objectContaining({ id: "job-1", updates: expect.any(Object) })));
    emit({ type: "started", job: job({ state: "downloading", error: null }) });
    await screen.findByRole("heading", { name: "Downloading" });
    await fireEvent.click(screen.getByRole("button", { name: "Cancel download" }));
    expect((await screen.findAllByText("The download could not be cancelled.")).length).toBeGreaterThan(0);
    finishResume({ job: job({ state: "interrupted", can_resume: true }), notice: null });
    await fireEvent.click(await screen.findByRole("button", { name: "Done" }));
    await fireEvent.click(within(screen.getByRole("banner", { name: "Download actions" })).getByRole("button", { name: "Add download" }));
    await fireEvent.click(screen.getByRole("button", { name: "Browse" }));
    await waitFor(() => expect((screen.getByLabelText("Destination folder") as HTMLInputElement).value).toBe("/tmp/gunda-test-output"));
    await fireEvent.input(screen.getByLabelText("File URL"), { target: { value: "https://example.test/fails.bin" } });
    await fireEvent.click(screen.getByRole("button", { name: "Download" }));
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("start_download", expect.objectContaining({ url: "https://example.test/fails.bin" })));
    expect(await screen.findByText("The file could not be started.")).toBeTruthy();
  });
});
