import { describe, expect, it } from "vitest";
import { formatBytes, percentage, totalBytes, type DownloadView } from "$lib/downloads";

function job(overrides: Partial<DownloadView> = {}): DownloadView {
  return {
    id: "job-1", name: "archive.zip", state: "downloading", written_bytes: "512", total_bytes: "1024",
    source: "https://example.test/archive.zip", added_via: "Desktop", created_at: "1791500000",
    content_type: null, output_path: null, error: null, can_resume: false, ...overrides,
  };
}

describe("download presentation helpers", () => {
  it("formats bytes beyond JavaScript's safe integer range", () => {
    expect(formatBytes("9007199254740992")).toBe("8.0 PiB");
    expect(formatBytes("1024")).toBe("1.0 KiB");
    expect(formatBytes("27")).toBe("27 B");
  });

  it("distinguishes known, unknown, and zero totals", () => {
    expect(percentage(job())).toBe(50);
    expect(percentage(job({ total_bytes: null }))).toBeNull();
    expect(percentage(job({ total_bytes: "0" }))).toBeNull();
  });

  it.each(["failed", "cancelled", "interrupted"] as const)("keeps partial progress for %s downloads", (state) => {
    expect(percentage(job({ state, written_bytes: "3", total_bytes: "8" }))).toBe(37.5);
  });

  it("uses the received size as the total for completed unknown-length downloads", () => {
    const completed = job({ state: "completed", written_bytes: "1234", total_bytes: null });
    expect(totalBytes(completed)).toBe("1234");
    expect(percentage(completed)).toBe(100);
  });
});
