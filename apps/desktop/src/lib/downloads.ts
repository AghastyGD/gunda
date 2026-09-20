export type DownloadView = {
  id: string;
  name: string;
  state: string;
  written_bytes: string;
  total_bytes: string | null;
  output_path: string | null;
  error: string | null;
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

export function formatBytes(value: string): string {
  const bytes = Number(value);

  if (bytes < 1024) {
    return `${bytes} B`;
  }

  const units = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
  let size = bytes / 1024;
  let index = 0;

  while (size >= 1024 && index < units.length - 1) {
    size /= 1024;
    index += 1;
  }

  return `${size.toFixed(1)} ${units[index]}`;
}

export function percentage(job: DownloadView): number | null {
  if (job.state === "completed") {
    return 100;
  }

  if (job.total_bytes === null || Number(job.total_bytes) === 0) {
    return null;
  }

  return Math.min(
    100,
    (Number(job.written_bytes) / Number(job.total_bytes)) * 100,
  );
}