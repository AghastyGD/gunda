type CandidateReport = {
  url: string;
  content_type: string | null;
  page_url: string;
  page_title: string | null;
  filename?: string | null;
};

type PageSource = {
  source: 'gunda-page';
  kind: 'source';
  source_url: string;
  document: unknown;
};

type PageMedia = {
  source: 'gunda-page';
  kind: 'candidate';
  url: string;
};

type PageRefresh = {
  source: 'gunda-page';
  kind: 'refresh';
};

type PageMessage = PageSource | PageMedia | PageRefresh;

type RuntimeMessage =
  | { kind: 'candidate'; url: string; content_type: string | null }
  | {
      kind: 'source';
      source_url: string;
      document: unknown;
    }
  | { kind: 'reconnect' }
  | { kind: 'forget' }
  | { kind: 'list-formats' }
  | { kind: 'download-status'; id: number }
  | {
      kind: 'open-download';
      capture_id: number;
      format: string;
      label: string;
      details: string;
      directory: string | null;
      filename: string | null;
    }
  | { kind: 'choose-folder' }
  | {
      kind: 'start-download';
      page_url: string;
      capture_id: number;
      format: string;
      directory: string;
      filename: string;
    };

type ServerMessage =
  | { kind: 'ready'; version: number; features?: string[] }
  | { kind: 'accepted'; id: number }
  | { kind: 'duplicate'; id: number }
  | { kind: 'recorded'; accepted: number[]; duplicates: number[] }
  | { kind: 'download_queued'; id: number }
  | { kind: 'download_unavailable'; reason: string }
  | { kind: 'download_status'; status: CaptureDownloadStatus }
  | { kind: 'download_status_unavailable'; reason: string }
  | {
      kind: 'page_formats';
      capture_id: number;
      formats: MediaFormat[];
      directory: string | null;
      filename: string | null;
    }
  | { kind: 'formats_unavailable'; reason: string }
  | { kind: 'refused'; reason: string }
  | { kind: 'malformed'; reason: string };

type ChannelStatus = {
  state: string;
  detail?: string;
  refused?: string;
  protocol?: number;
  at: number;
};

type ChannelStats = Record<string, number>;

type BrowserDownloadOutcome =
  | 'intercepted'
  | 'kept'
  | 'ignored_extension'
  | 'ignored_non_http';

type BrowserDownloadDiagnostic = {
  at: number;
  scheme: string;
  host: string;
  mime: string;
  filename: string | null;
  by_extension: boolean;
  outcome: BrowserDownloadOutcome;
  reason: string | null;
};

type PairingSettings = {
  endpoint: string;
  token: string;
};

type DownloadResponse =
  | { kind: 'queued'; id: number }
  | { kind: 'unavailable'; reason: string };

type MediaFormat = {
  label: string;
  group?: string | null;
  details: string;
  format: string;
};

type FormatResponse =
  | {
      kind: 'formats';
      capture_id: number;
      formats: MediaFormat[];
      directory: string | null;
      filename: string | null;
    }
  | { kind: 'unavailable'; reason: string };

type CaptureDownloadStatus = {
  id: number;
  name: string;
  state: string;
  written: number;
  total: number | null;
  failure: string | null;
};

type DownloadStatusResponse =
  | { kind: 'status'; status: CaptureDownloadStatus }
  | { kind: 'unavailable'; reason: string };

interface Window {
  __gundaPageAgent?: boolean;
}

interface XMLHttpRequest {
  __gundaUrl?: string;
}
