//! What the browser reported, and the bounded list of what was kept.
//!
//! A report is validated before it becomes a candidate. Validation is the only
//! place that decides what Gunda is willing to fetch, so it is deliberately
//! strict: a bad URL, an unrecognised media type, or a single stream segment is
//! refused with a typed reason instead of being handed to the download engine
//! to fail later.

use std::time::Instant;

use gunda_core::download::{DownloadOrigin, ResourceKind};
use url::Url;

use crate::protocol::Refusal;

/// How many candidates are kept before the oldest is evicted.
pub const DEFAULT_CAPACITY: usize = 256;

/// Largest page title kept, in characters.
const MAX_TITLE_CHARS: usize = 200;

/// Largest content type kept, in bytes.
const MAX_CONTENT_TYPE_BYTES: usize = 128;

/// Identifier of a captured candidate.
pub type CandidateId = u64;

/// A validated report from the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    url: Url,
    kind: ResourceKind,
    content_type: Option<String>,
    page_url: Url,
    page_title: Option<String>,
}

impl Report {
    /// Validates a raw report from the extension.
    ///
    /// # Errors
    ///
    /// Returns the reason the report cannot become a candidate.
    pub fn new(
        url: &str,
        content_type: Option<&str>,
        page_url: &str,
        page_title: Option<&str>,
    ) -> Result<Self, Refusal> {
        let url = Url::parse(url).map_err(|_| Refusal::UnsupportedUrl)?;

        if !matches!(url.scheme(), "http" | "https") {
            return Err(Refusal::UnsupportedUrl);
        }

        let page_url = Url::parse(page_url).map_err(|_| Refusal::UnsupportedUrl)?;

        if page_url.cannot_be_a_base() {
            return Err(Refusal::UnsupportedUrl);
        }

        let content_type = content_type.map(|value| truncate(value, MAX_CONTENT_TYPE_BYTES));
        let kind = match classify(&url, content_type.as_deref()) {
            Classification::Media(kind) => kind,
            Classification::Segment => return Err(Refusal::SegmentStream),
            Classification::Unknown => return Err(Refusal::UnsupportedMedia),
        };

        let page_title = page_title
            .map(clean_title)
            .filter(|title| !title.is_empty());

        Ok(Self {
            url,
            kind,
            content_type,
            page_url,
            page_title,
        })
    }
}

/// One media resource the browser reported.
#[derive(Debug, Clone)]
pub struct Candidate {
    id: CandidateId,
    url: Url,
    kind: ResourceKind,
    content_type: Option<String>,
    page_url: Url,
    page_title: Option<String>,
    first_seen: Instant,
    last_seen: Instant,
    seen: u32,
}

impl Candidate {
    /// The candidate's identifier, unique within one catalog.
    #[must_use]
    pub const fn id(&self) -> CandidateId {
        self.id
    }

    #[must_use]
    pub const fn url(&self) -> &Url {
        &self.url
    }

    #[must_use]
    pub const fn kind(&self) -> ResourceKind {
        self.kind
    }

    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    #[must_use]
    pub const fn page_url(&self) -> &Url {
        &self.page_url
    }

    #[must_use]
    pub fn page_title(&self) -> Option<&str> {
        self.page_title.as_deref()
    }

    /// The final path segment of the media URL, still percent-encoded.
    ///
    /// This is for display only. The download engine derives the destination
    /// file name, and it decodes and sanitises it in one place.
    #[must_use]
    pub fn path_segment(&self) -> Option<&str> {
        self.url.path_segments()?.next_back()
    }

    /// The origin to attribute a download from this candidate to.
    #[must_use]
    pub fn origin(&self) -> DownloadOrigin {
        DownloadOrigin::Browser {
            page_url: self.page_url.clone(),
            page_title: self.page_title.clone(),
        }
    }

    #[must_use]
    pub const fn first_seen(&self) -> Instant {
        self.first_seen
    }

    #[must_use]
    pub const fn last_seen(&self) -> Instant {
        self.last_seen
    }

    #[must_use]
    pub const fn seen(&self) -> u32 {
        self.seen
    }
}

/// The result of recording a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    pub id: CandidateId,
    pub duplicate: bool,
}

/// The candidates seen so far, newest first.
///
/// The list is bounded: once it is full the oldest entry is evicted, so a long
/// browsing session can never grow it without limit and the most recent
/// discovery is never the one that gets lost. A candidate that produced a
/// download is unaffected by eviction; the job manager owns it from then on.
#[derive(Debug)]
pub struct Catalog {
    candidates: Vec<Candidate>,
    next_id: CandidateId,
    capacity: usize,
}

impl Catalog {
    /// Creates a catalog that keeps at most `capacity` candidates.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            candidates: Vec::new(),
            next_id: 0,
            capacity: capacity.max(1),
        }
    }

    /// Records a validated report.
    ///
    /// A report for a URL that is already known does not create a second
    /// candidate: it updates that candidate's content type, last-seen time, and
    /// report count.
    pub fn record(&mut self, report: Report) -> Recorded {
        let now = Instant::now();

        if let Some(existing) = self
            .candidates
            .iter_mut()
            .find(|candidate| candidate.url == report.url)
        {
            existing.last_seen = now;
            existing.seen = existing.seen.saturating_add(1);

            // A later report of the same URL often knows more than the first
            // one (a media request carries a type; a prediction may not).
            if report.content_type.is_some() {
                existing.content_type = report.content_type;
            }

            return Recorded {
                id: existing.id,
                duplicate: true,
            };
        }

        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);

        self.candidates.insert(
            0,
            Candidate {
                id,
                url: report.url,
                kind: report.kind,
                content_type: report.content_type,
                page_url: report.page_url,
                page_title: report.page_title,
                first_seen: now,
                last_seen: now,
                seen: 1,
            },
        );

        self.candidates.truncate(self.capacity);

        Recorded {
            id,
            duplicate: false,
        }
    }

    #[must_use]
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Finds a candidate by identifier.
    #[must_use]
    pub fn get(&self, id: CandidateId) -> Option<&Candidate> {
        self.candidates.iter().find(|candidate| candidate.id == id)
    }

    pub fn remove(&mut self, id: CandidateId) -> bool {
        let before = self.candidates.len();
        self.candidates.retain(|candidate| candidate.id != id);

        self.candidates.len() != before
    }

    pub fn clear(&mut self) {
        self.candidates.clear();
    }
}

/// What a reported resource turned out to be.
enum Classification {
    /// Something the download engine can fetch.
    Media(ResourceKind),
    /// One piece of a segmented stream.
    Segment,
    /// Not media this build recognises.
    Unknown,
}

/// Classifies a reported resource by its content type, then by its path.
///
/// The content type is authoritative when it is decisive. `application/
/// octet-stream` is not: plenty of servers label video that way, so a path that
/// names a media container is still accepted.
fn classify(url: &Url, content_type: Option<&str>) -> Classification {
    if range_query(url) {
        return Classification::Segment;
    }

    if let Some(content_type) = content_type {
        let essence = content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();

        match essence.as_str() {
            "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "audio/mpegurl"
            | "audio/x-mpegurl" => return Classification::Media(ResourceKind::Hls),
            "application/dash+xml" => return Classification::Media(ResourceKind::Dash),
            // A transport stream is what an HLS segment is served as. The
            // manifest is the resource worth downloading, and reporting every
            // segment would bury it.
            "video/mp2t" => return Classification::Segment,
            _ => {}
        }

        if essence.starts_with("video/") || essence.starts_with("audio/") {
            return Classification::Media(ResourceKind::File);
        }
    }

    match extension(url).as_deref() {
        Some("m3u8") => Classification::Media(ResourceKind::Hls),
        Some("mpd") => Classification::Media(ResourceKind::Dash),
        Some(
            "mp4" | "m4v" | "webm" | "mkv" | "mov" | "mp3" | "m4a" | "aac" | "ogg" | "oga" | "opus"
            | "flac" | "wav",
        ) => Classification::Media(ResourceKind::File),
        Some("ts" | "m4s") => Classification::Segment,
        _ => Classification::Unknown,
    }
}

fn range_query(url: &Url) -> bool {
    url.query_pairs().any(|(name, _)| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "bytestart" | "byteend" | "range"
        )
    })
}

/// The lowercase extension of a URL's final path segment.
fn extension(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.next_back()?;
    let (_, extension) = segment.rsplit_once('.')?;
    let extension = extension.to_ascii_lowercase();

    if extension.is_empty() || extension.len() > 5 {
        return None;
    }

    Some(extension)
}

/// Drops control characters and bounds the length of a page title.
fn clean_title(title: &str) -> String {
    title
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Truncates a string to a byte budget, on a character boundary.
fn truncate(value: &str, bytes: usize) -> String {
    if value.len() <= bytes {
        return value.to_owned();
    }

    let mut end = bytes;

    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }

    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use gunda_core::download::{DownloadOrigin, ResourceKind};

    use super::{Catalog, DEFAULT_CAPACITY, Report};
    use crate::protocol::Refusal;

    fn report(url: &str) -> Report {
        Report::new(url, None, "https://example.com/watch", None).expect("the report is valid")
    }

    #[test]
    fn classifies_a_manifest_from_its_content_type() {
        let report = Report::new(
            "https://cdn.example.com/play",
            Some("application/vnd.apple.mpegurl; charset=utf-8"),
            "https://example.com/watch",
            None,
        )
        .expect("the report is valid");

        assert_eq!(report.kind, ResourceKind::Hls);
    }

    #[test]
    fn classifies_a_manifest_from_its_path() {
        assert_eq!(
            report("https://cdn.example.com/stream.m3u8?token=x").kind,
            ResourceKind::Hls
        );
        assert_eq!(
            report("https://cdn.example.com/stream.mpd").kind,
            ResourceKind::Dash
        );
    }

    #[test]
    fn accepts_media_served_as_octet_stream() {
        let report = Report::new(
            "https://cdn.example.com/movie.mp4",
            Some("application/octet-stream"),
            "https://example.com/watch",
            None,
        )
        .expect("the report is valid");

        assert_eq!(report.kind, ResourceKind::File);
    }

    #[test]
    fn refuses_a_url_that_is_not_http() {
        assert_eq!(
            Report::new(
                "ftp://cdn.example.com/x.mp4",
                None,
                "https://example.com",
                None
            )
            .expect_err("only http is fetched"),
            Refusal::UnsupportedUrl
        );
        assert_eq!(
            Report::new("not a url", None, "https://example.com", None)
                .expect_err("a URL must parse"),
            Refusal::UnsupportedUrl
        );
        assert_eq!(
            Report::new("https://cdn.example.com/x.mp4", None, "about:blank", None)
                .expect_err("the page URL must be absolute"),
            Refusal::UnsupportedUrl
        );
    }

    #[test]
    fn refuses_a_stream_segment() {
        assert_eq!(
            Report::new(
                "https://cdn.example.com/segment-9.ts",
                Some("video/mp2t"),
                "https://example.com/watch",
                None
            )
            .expect_err("segments are not captured"),
            Refusal::SegmentStream
        );
        assert_eq!(
            Report::new(
                "https://cdn.example.com/video.mp4?bytestart=0&byteend=1023",
                Some("video/mp4"),
                "https://example.com/watch",
                None,
            )
            .expect_err("a range-marked MP4 is a fragment"),
            Refusal::SegmentStream
        );
    }

    #[test]
    fn refuses_something_that_is_not_media() {
        assert_eq!(
            Report::new(
                "https://example.com/app.js",
                Some("text/javascript"),
                "https://example.com/watch",
                None
            )
            .expect_err("scripts are not media"),
            Refusal::UnsupportedMedia
        );
    }

    #[test]
    fn bounds_the_page_title() {
        let title = format!("{}\n\tafter", "x".repeat(400));

        let report = Report::new(
            "https://cdn.example.com/movie.mp4",
            Some("video/mp4"),
            "https://example.com/watch",
            Some(&title),
        )
        .expect("the report is valid");

        let page_title = report.page_title.expect("a title was kept");

        assert_eq!(page_title.chars().count(), 200);
        assert!(page_title.chars().all(|character| !character.is_control()));
    }

    #[test]
    fn records_a_report_as_a_new_candidate() {
        let mut catalog = Catalog::new(DEFAULT_CAPACITY);

        let recorded = catalog.record(report("https://cdn.example.com/movie.mp4"));

        assert_eq!(recorded.id, 0);
        assert!(!recorded.duplicate);
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog.candidates()[0].url().as_str(),
            "https://cdn.example.com/movie.mp4"
        );
        assert_eq!(catalog.candidates()[0].path_segment(), Some("movie.mp4"));
    }

    #[test]
    fn a_repeated_report_updates_one_candidate() {
        let mut catalog = Catalog::new(DEFAULT_CAPACITY);

        let first = catalog.record(
            Report::new(
                "https://cdn.example.com/movie.mp4",
                None,
                "https://example.com/watch",
                None,
            )
            .expect("the report is valid"),
        );

        let second = catalog.record(
            Report::new(
                "https://cdn.example.com/movie.mp4",
                Some("video/mp4"),
                "https://example.com/watch",
                None,
            )
            .expect("the report is valid"),
        );

        assert_eq!(first.id, second.id);
        assert!(second.duplicate);
        assert_eq!(catalog.len(), 1);

        let candidate = &catalog.candidates()[0];

        assert_eq!(candidate.seen(), 2);
        assert!(candidate.last_seen() >= candidate.first_seen());
        assert_eq!(candidate.content_type(), Some("video/mp4"));
    }

    #[test]
    fn keeps_the_newest_candidates_within_capacity() {
        let mut catalog = Catalog::new(2);

        catalog.record(report("https://cdn.example.com/1.mp4"));
        catalog.record(report("https://cdn.example.com/2.mp4"));
        catalog.record(report("https://cdn.example.com/3.mp4"));

        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog.capacity(), 2);

        let urls = catalog
            .candidates()
            .iter()
            .map(|candidate| candidate.url().path().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(urls, vec!["/3.mp4", "/2.mp4"]);
    }

    #[test]
    fn forgets_candidates_by_identifier() {
        let mut catalog = Catalog::new(DEFAULT_CAPACITY);

        let recorded = catalog.record(report("https://cdn.example.com/movie.mp4"));

        assert!(catalog.remove(recorded.id));
        assert!(!catalog.remove(recorded.id));
        assert!(catalog.is_empty());
        assert!(catalog.get(recorded.id).is_none());
    }

    #[test]
    fn attributes_a_candidate_to_its_page() {
        let mut catalog = Catalog::new(DEFAULT_CAPACITY);

        catalog.record(
            Report::new(
                "https://cdn.example.com/movie.mp4",
                Some("video/mp4"),
                "https://example.com/watch",
                Some("A movie"),
            )
            .expect("the report is valid"),
        );

        let origin = catalog.candidates()[0].origin();

        let DownloadOrigin::Browser {
            page_url,
            page_title,
        } = origin
        else {
            panic!("a capture is attributed to the browser");
        };

        assert_eq!(page_url.as_str(), "https://example.com/watch");
        assert_eq!(page_title.as_deref(), Some("A movie"));
    }
}
