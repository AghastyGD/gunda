type CandidateReport = {
  url: string;
  content_type: string | null;
  page_url: string;
  page_title: string | null;
};

type PageCandidate = {
  source: 'gunda-page';
  kind: 'candidate';
  url: string;
};

type RuntimeMessage =
  | { kind: 'candidate'; url: string; content_type: string | null }
  | { kind: 'reconnect' }
  | { kind: 'start-page-agent' };

type ServerMessage =
  | { kind: 'ready'; version: number }
  | { kind: 'accepted'; id: number }
  | { kind: 'duplicate'; id: number }
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

interface Window {
  __gundaPageAgent?: boolean;
}

interface XMLHttpRequest {
  __gundaUrl?: string;
}
