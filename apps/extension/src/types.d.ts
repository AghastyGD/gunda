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

type PageMessage = PageSource | PageMedia;

type RuntimeMessage =
  | { kind: 'candidate'; url: string; content_type: string | null }
  | {
      kind: 'source';
      source_url: string;
      document: unknown;
    }
  | { kind: 'reconnect' };

type ServerMessage =
  | { kind: 'ready'; version: number }
  | { kind: 'accepted'; id: number }
  | { kind: 'duplicate'; id: number }
  | { kind: 'recorded'; accepted: number[]; duplicates: number[] }
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

interface Window {
  __gundaPageAgent?: boolean;
}

interface XMLHttpRequest {
  __gundaUrl?: string;
}
