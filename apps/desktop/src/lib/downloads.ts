export const downloadStates = [
  "queued",
  "inspecting",
  "downloading",
  "paused",
  "finalizing",
  "completed",
  "failed",
  "cancelled",
  "interrupted",
] as const;

export type DownloadState = (typeof downloadStates)[number];

export type DownloadView = {
  id: string;
  name: string;
  state: DownloadState;
  written_bytes: string;
  total_bytes: string | null;
  source: string;
  added_via: string;
  created_at: string;
  content_type: string | null;
  output_path: string | null;
  error: string | null;
  can_resume: boolean;
};

export type DownloadUpdate =
  | {
      type: "started";
      job: DownloadView;
    }
  | {
      type: "progress";
      id: string;
      written_bytes: string;
      total_bytes: string | null;
    };

export type ExecutionResponse = {
  job: DownloadView;
  notice: string | null;
};

export type DownloadFilter =
  | "all"
  | "incomplete"
  | "completed";

export function formatBytes(value: string): string {
  const bytes = BigInt(value);

  if (bytes < 1024n) {
    return `${bytes} B`;
  }

  const units = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
  let divisor = 1024n;
  let unit = 0;

  while (unit < units.length - 1 && bytes >= divisor * 1024n) {
    divisor *= 1024n;
    unit += 1;
  }

  const tenths = (bytes * 10n) / divisor;
  return `${tenths / 10n}.${tenths % 10n} ${units[unit]}`;
}

export function percentage(job: DownloadView): number | null {
  if (job.state === "completed") {
    return 100;
  }

  if (job.total_bytes === null) {
    return null;
  }

  const total = BigInt(job.total_bytes);
  if (total === 0n) {
    return null;
  }

  const written = BigInt(job.written_bytes);
  const tenths = (written * 1000n) / total;
  return Number(tenths > 1000n ? 1000n : tenths) / 10;
}

export function totalBytes(job: DownloadView): string | null {
  return job.total_bytes ?? (job.state === "completed" ? job.written_bytes : null);
}

export function stateLabel(state: DownloadState): string {
  const labels: Record<DownloadState, string> = {
    queued: "Waiting",
    inspecting: "Preparing",
    downloading: "Downloading",
    paused: "Paused",
    finalizing: "Finishing",
    completed: "Completed",
    failed: "Failed",
    cancelled: "Cancelled",
    interrupted: "Interrupted",
  };

  return labels[state];
}

export function formatAddedTime(value: string): string {
  const date = new Date(Number(value) * 1000);

  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(date);
}
