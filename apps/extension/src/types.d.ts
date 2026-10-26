type CandidateReport = {
  url: string;
  content_type: string | null;
  page_url: string;
  page_title: string | null;
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
  | { kind: 'list-formats' }
  | { kind: 'download-page'; format?: string };

type ServerMessage =
  | { kind: 'ready'; version: number }
  | { kind: 'accepted'; id: number }
  | { kind: 'duplicate'; id: number }
  | { kind: 'recorded'; accepted: number[]; duplicates: number[] }
  | { kind: 'download_queued'; id: number }
  | { kind: 'download_unavailable'; reason: string }
  | { kind: 'page_formats'; capture_id: number; formats: MediaFormat[] }
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

type PairingSettings = {
  endpoint: string;
  token: string;
};

type DownloadResponse =
  | { kind: 'queued'; id: number }
  | { kind: 'unavailable'; reason: string };

type MediaFormat = {
  label: string;
  details: string;
  format: string;
};

type FormatResponse =
  | { kind: 'formats'; capture_id: number; formats: MediaFormat[] }
  | { kind: 'unavailable'; reason: string };

interface Window {
  __gundaPageAgent?: boolean;
}

interface XMLHttpRequest {
  __gundaUrl?: string;
}
