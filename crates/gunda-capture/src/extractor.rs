//! Bounded extraction of media presentations from captured page JSON.
//!
//! Site adapters only recognise the shape a site uses to describe media. URL
//! validation, variant limits, ordering, and candidate creation stay in the
//! daemon so browser-side code cannot inject unvalidated downloads.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use url::Url;

use crate::protocol::Refusal;

/// Largest source value depth inspected by one extraction.
const MAX_DEPTH: usize = 32;

/// Largest source value count inspected by one extraction.
const MAX_NODES: usize = 50_000;

/// Largest number of presentations returned by one source.
const MAX_PRESENTATIONS: usize = 16;

/// Largest number of progressive variants retained for one presentation.
const MAX_VARIANTS: usize = 24;

/// Largest media URL accepted from a source.
const MAX_URL_BYTES: usize = 8 * 1024;

/// Largest title retained for one presentation.
const MAX_TITLE_CHARS: usize = 200;

/// One directly downloadable quality of a presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaVariant {
    url: Url,
    content_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    bitrate: Option<u64>,
    codecs: Option<String>,
    label: Option<String>,
    rank: u8,
}

impl MediaVariant {
    #[must_use]
    pub const fn url(&self) -> &Url {
        &self.url
    }

    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    #[must_use]
    pub const fn resolution(&self) -> Option<(u32, u32)> {
        match (self.width, self.height) {
            (Some(width), Some(height)) => Some((width, height)),
            _ => None,
        }
    }

    #[must_use]
    pub const fn bitrate(&self) -> Option<u64> {
        self.bitrate
    }

    #[must_use]
    pub fn codecs(&self) -> Option<&str> {
        self.codecs.as_deref()
    }

    /// A short label suitable for a quality list.
    #[must_use]
    pub fn label(&self) -> String {
        if let Some(label) = &self.label {
            return label.clone();
        }

        if let Some((width, height)) = self.resolution() {
            return format!("{width}x{height}");
        }

        if let Some(codecs) = &self.codecs {
            return codecs.clone();
        }

        String::from("Media")
    }
}

/// One presentation found in a captured page document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedPresentation {
    pub(crate) url: Url,
    pub(crate) content_type: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) variants: Vec<MediaVariant>,
}

impl ExtractedPresentation {
    #[must_use]
    pub const fn url(&self) -> &Url {
        &self.url
    }

    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    #[must_use]
    pub fn variants(&self) -> &[MediaVariant] {
        &self.variants
    }
}

/// Extracts supported presentations from one bounded JSON response.
///
/// # Errors
///
/// Returns [`Refusal::UnsupportedSource`] for an unsupported page and
/// [`Refusal::UnsupportedMedia`] when the document contains no media.
pub fn extract(
    page_url: &Url,
    source_url: &Url,
    document: &Value,
) -> Result<Vec<ExtractedPresentation>, Refusal> {
    if !matches!(source_url.scheme(), "http" | "https") {
        return Err(Refusal::UnsupportedUrl);
    }

    let platform = Platform::of(page_url).ok_or(Refusal::UnsupportedSource)?;
    let mut collector = Collector::new(platform, source_url);
    collector.visit(document);
    collector.finish()
}

#[derive(Debug, Clone, Copy)]
enum Platform {
    Instagram,
    Facebook,
    YouTube,
}

impl Platform {
    fn of(page_url: &Url) -> Option<Self> {
        let host = page_url.host_str()?.to_ascii_lowercase();

        if host == "instagram.com" || host.ends_with(".instagram.com") {
            Some(Self::Instagram)
        } else if host == "facebook.com" || host.ends_with(".facebook.com") {
            Some(Self::Facebook)
        } else if host == "youtube.com" || host.ends_with(".youtube.com") || host == "youtu.be" {
            Some(Self::YouTube)
        } else {
            None
        }
    }
}

struct Collector<'a> {
    platform: Platform,
    source_url: &'a Url,
    presentations: BTreeMap<String, PresentationBuilder>,
    nodes: usize,
}

impl<'a> Collector<'a> {
    fn new(platform: Platform, source_url: &'a Url) -> Self {
        Self {
            platform,
            source_url,
            presentations: BTreeMap::new(),
            nodes: 0,
        }
    }

    fn visit(&mut self, document: &Value) {
        let mut pending = vec![(document, 0_usize)];

        while let Some((value, depth)) = pending.pop() {
            if self.nodes >= MAX_NODES
                || self.presentations.len() >= MAX_PRESENTATIONS
                || depth > MAX_DEPTH
            {
                break;
            }

            self.nodes = self.nodes.saturating_add(1);

            match value {
                Value::Array(items) => {
                    if depth < MAX_DEPTH {
                        pending.extend(items.iter().map(|item| (item, depth + 1)));
                    }
                }
                Value::Object(object) => {
                    self.consider(object);
                    if depth < MAX_DEPTH {
                        pending.extend(
                            object
                                .values()
                                .map(|child| (child, depth.saturating_add(1))),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn consider(&mut self, object: &Map<String, Value>) {
        match self.platform {
            Platform::Instagram => self.consider_instagram(object),
            Platform::Facebook => self.consider_facebook(object),
            Platform::YouTube => self.consider_youtube(object),
        }
    }

    fn consider_instagram(&mut self, object: &Map<String, Value>) {
        let mut variants = Vec::new();

        if let Some(versions) = object.get("video_versions").and_then(Value::as_array) {
            for version in versions {
                if let Some(version) = version.as_object() {
                    add_variant(
                        &mut variants,
                        version.get("url"),
                        Some("video/mp4"),
                        version,
                    );
                }
            }
        }

        if let Some(resources) = object.get("video_resources").and_then(Value::as_array) {
            for resource in resources {
                if let Some(resource) = resource.as_object() {
                    add_variant(
                        &mut variants,
                        resource.get("src"),
                        Some("video/mp4"),
                        resource,
                    );
                }
            }
        }

        add_variant(
            &mut variants,
            object.get("video_url"),
            Some("video/mp4"),
            object,
        );

        if !variants.is_empty() {
            self.add_presentation(object, variants, None, instagram_title(object));
        }
    }

    fn consider_facebook(&mut self, object: &Map<String, Value>) {
        let mut variants = Vec::new();

        add_labeled_variant(
            &mut variants,
            object.get("browser_native_hd_url"),
            Some("video/mp4"),
            object,
            "HD",
            2,
        );
        add_labeled_variant(
            &mut variants,
            object.get("playable_url_quality_hd"),
            Some("video/mp4"),
            object,
            "HD",
            2,
        );
        add_labeled_variant(
            &mut variants,
            object.get("browser_native_sd_url"),
            Some("video/mp4"),
            object,
            "SD",
            1,
        );
        add_labeled_variant(
            &mut variants,
            object.get("playable_url"),
            Some("video/mp4"),
            object,
            "SD",
            1,
        );

        if let Some(progressive) = object.get("progressive_urls").and_then(Value::as_array) {
            for item in progressive {
                if let Some(item) = item.as_object() {
                    add_variant(&mut variants, item.get("url"), Some("video/mp4"), item);
                }
            }
        }

        if !variants.is_empty() {
            self.add_presentation(object, variants, None, None);
        }
    }

    fn consider_youtube(&mut self, object: &Map<String, Value>) {
        let Some(streaming) = object.get("streamingData").and_then(Value::as_object) else {
            return;
        };
        let details = object.get("videoDetails").and_then(Value::as_object);
        let mut manifest = None;

        if let Some(data) = streaming.get("hlsManifestUrl") {
            let mut variants = Vec::new();
            add_variant(
                &mut variants,
                Some(data),
                Some("application/vnd.apple.mpegurl"),
                object,
            );
            manifest = variants.into_iter().next();
        }

        let mut variants = Vec::new();
        if let Some(formats) = streaming.get("formats").and_then(Value::as_array) {
            for format in formats {
                if let Some(format) = format.as_object() {
                    add_variant(&mut variants, format.get("url"), None, format);
                }
            }
        }

        if variants.is_empty() && manifest.is_none() {
            return;
        }

        let title = details
            .and_then(|details| details.get("title"))
            .and_then(Value::as_str)
            .map(clean_title)
            .filter(|title| !title.is_empty());
        self.add_presentation(object, variants, manifest, title);
    }

    fn add_presentation(
        &mut self,
        object: &Map<String, Value>,
        variants: Vec<MediaVariant>,
        manifest: Option<MediaVariant>,
        title: Option<String>,
    ) {
        let key = presentation_key(object, self.source_url, &variants)
            .or_else(|| manifest.as_ref().map(|item| item.url.as_str().to_owned()))
            .unwrap_or_else(|| self.source_url.as_str().to_owned());

        let builder = self.presentations.entry(key).or_default();

        if builder.title.is_none() {
            builder.title = title;
        }
        if builder.manifest.is_none() {
            builder.manifest = manifest;
        }
        for variant in variants {
            builder.add_variant(variant);
        }
    }

    fn finish(self) -> Result<Vec<ExtractedPresentation>, Refusal> {
        let mut presentations = self
            .presentations
            .into_values()
            .filter_map(PresentationBuilder::finish)
            .collect::<Vec<_>>();

        presentations.sort_by(|left, right| {
            right
                .variants
                .len()
                .cmp(&left.variants.len())
                .then_with(|| left.url.as_str().cmp(right.url.as_str()))
        });

        if presentations.is_empty() {
            Err(Refusal::UnsupportedMedia)
        } else {
            Ok(presentations)
        }
    }
}

#[derive(Default)]
struct PresentationBuilder {
    variants: Vec<MediaVariant>,
    manifest: Option<MediaVariant>,
    title: Option<String>,
}

impl PresentationBuilder {
    fn add_variant(&mut self, variant: MediaVariant) {
        if self.variants.len() >= MAX_VARIANTS {
            return;
        }

        if let Some(existing) = self
            .variants
            .iter_mut()
            .find(|existing| existing.url == variant.url)
        {
            existing.enrich(variant);
            return;
        }

        self.variants.push(variant);
    }

    fn finish(mut self) -> Option<ExtractedPresentation> {
        if let Some(manifest) = self.manifest {
            return Some(ExtractedPresentation {
                url: manifest.url,
                content_type: manifest.content_type,
                title: self.title,
                variants: Vec::new(),
            });
        }

        self.variants.sort_by(|left, right| {
            variant_area(right)
                .cmp(&variant_area(left))
                .then_with(|| right.rank.cmp(&left.rank))
                .then_with(|| right.bitrate.cmp(&left.bitrate))
                .then_with(|| left.url.as_str().cmp(right.url.as_str()))
        });
        let best = self.variants.first()?.clone();

        Some(ExtractedPresentation {
            url: best.url,
            content_type: best.content_type,
            title: self.title,
            variants: self.variants,
        })
    }
}

impl MediaVariant {
    fn enrich(&mut self, other: Self) {
        self.content_type = other.content_type.or(self.content_type.take());
        self.width = other.width.or(self.width);
        self.height = other.height.or(self.height);
        self.bitrate = other.bitrate.or(self.bitrate);
        self.codecs = other.codecs.or(self.codecs.take());
        self.label = other.label.or(self.label.take());
        self.rank = self.rank.max(other.rank);
    }
}

fn add_labeled_variant(
    variants: &mut Vec<MediaVariant>,
    url: Option<&Value>,
    content_type: Option<&str>,
    metadata: &Map<String, Value>,
    label: &str,
    rank: u8,
) {
    if let Some(mut variant) = media_variant(url, content_type, metadata) {
        variant.label = Some(label.to_owned());
        variant.rank = rank;
        variants.push(variant);
    }
}

fn add_variant(
    variants: &mut Vec<MediaVariant>,
    url: Option<&Value>,
    content_type: Option<&str>,
    metadata: &Map<String, Value>,
) {
    if let Some(variant) = media_variant(url, content_type, metadata) {
        variants.push(variant);
    }
}

fn media_variant(
    url: Option<&Value>,
    default_content_type: Option<&str>,
    metadata: &Map<String, Value>,
) -> Option<MediaVariant> {
    let raw = url?.as_str()?;
    if raw.len() > MAX_URL_BYTES {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }

    let content_type = string_field(metadata, &["mimeType", "mime_type", "content_type"])
        .or_else(|| default_content_type.map(str::to_owned));
    if is_fragment(&url, content_type.as_deref()) {
        return None;
    }
    let codecs = string_field(metadata, &["codecs"])
        .or_else(|| content_type.as_deref().and_then(codecs_from_mime));

    Some(MediaVariant {
        url,
        content_type,
        width: integer_field(metadata, &["width", "config_width"])
            .and_then(|value| u32::try_from(value).ok()),
        height: integer_field(metadata, &["height", "config_height"])
            .and_then(|value| u32::try_from(value).ok()),
        bitrate: integer_field(metadata, &["bitrate", "bit_rate", "bandwidth"]),
        codecs,
        label: string_field(metadata, &["quality_label", "quality"]),
        rank: 0,
    })
}

fn presentation_key(
    object: &Map<String, Value>,
    source_url: &Url,
    variants: &[MediaVariant],
) -> Option<String> {
    for name in [
        "pk",
        "video_id",
        "videoId",
        "media_id",
        "id",
        "shortcode",
        "code",
    ] {
        if let Some(value) = object.get(name)
            && let Some(value) = scalar_string(value)
        {
            return Some(format!("{name}:{value}"));
        }
    }

    if let Some(details) = object.get("videoDetails").and_then(Value::as_object)
        && let Some(id) = details.get("videoId").and_then(scalar_string)
    {
        return Some(format!("videoId:{id}"));
    }

    variants
        .first()
        .map(|variant| variant.url.as_str().to_owned())
        .or_else(|| Some(source_url.as_str().to_owned()))
}

fn instagram_title(object: &Map<String, Value>) -> Option<String> {
    if let Some(title) = object.get("title").and_then(Value::as_str) {
        let title = clean_title(title);
        if !title.is_empty() {
            return Some(title);
        }
    }

    if let Some(caption) = object.get("caption") {
        let caption = caption.as_str().map(str::to_owned).or_else(|| {
            caption
                .as_object()?
                .get("text")?
                .as_str()
                .map(str::to_owned)
        });
        if let Some(caption) = caption {
            let caption = clean_title(&caption);
            if !caption.is_empty() {
                return Some(caption);
            }
        }
    }

    object
        .get("edge_media_to_caption")
        .and_then(Value::as_object)
        .and_then(|caption| caption.get("edges"))
        .and_then(Value::as_array)
        .and_then(|edges| edges.first())
        .and_then(Value::as_object)
        .and_then(|edge| edge.get("node"))
        .and_then(Value::as_object)
        .and_then(|node| node.get("text"))
        .and_then(Value::as_str)
        .map(clean_title)
        .filter(|title| !title.is_empty())
        .or_else(|| {
            object
                .get("highlights_info")
                .and_then(Value::as_object)
                .and_then(|highlights| highlights.get("title"))
                .and_then(Value::as_str)
                .map(clean_title)
                .filter(|title| !title.is_empty())
        })
        .or_else(|| {
            object
                .get("song_name")
                .and_then(Value::as_str)
                .map(clean_title)
                .filter(|title| !title.is_empty())
        })
}

fn string_field(object: &Map<String, Value>, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str))
        .map(str::to_owned)
}

fn integer_field(object: &Map<String, Value>, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        let value = object.get(*name)?;
        value
            .as_u64()
            .or_else(|| value.as_str()?.parse::<u64>().ok())
    })
}

fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn codecs_from_mime(content_type: &str) -> Option<String> {
    let (_, codecs) = content_type.split_once("codecs=")?;
    Some(codecs.trim_matches(['"', '\'']).to_owned())
}

fn is_fragment(url: &Url, content_type: Option<&str>) -> bool {
    if url.query_pairs().any(|(name, _)| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "bytestart" | "byteend" | "range"
        )
    }) {
        return true;
    }

    if content_type.is_some_and(|content_type| {
        matches!(
            content_type
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "video/mp2t" | "video/iso.segment"
        )
    }) {
        return true;
    }

    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(|segment| segment.rsplit_once('.').map(|(_, extension)| extension))
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("ts") || extension.eq_ignore_ascii_case("m4s")
        })
}

fn variant_area(variant: &MediaVariant) -> u64 {
    variant
        .resolution()
        .map_or(0, |(width, height)| u64::from(width) * u64::from(height))
}

fn clean_title(title: &str) -> String {
    title
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use url::Url;

    use super::extract;
    use crate::protocol::Refusal;

    fn page(value: &str) -> Url {
        Url::parse(value).expect("the page URL is valid")
    }

    #[test]
    fn extracts_every_instagram_progressive_quality() {
        let document = json!({
            "data": {
                "media": {
                    "pk": "media-7",
                    "caption": {"text": "A reel"},
                    "video_versions": [
                        {"url": "https://cdn.example/low.mp4", "width": 640, "height": 360},
                        {"url": "https://cdn.example/fragment.mp4?bytestart=0&byteend=99", "width": 2160, "height": 3840},
                        {"url": "https://cdn.example/segment.ts", "width": 2160, "height": 3840},
                        {"url": "https://cdn.example/high.mp4", "width": 1080, "height": 1920}
                    ]
                }
            }
        });

        let presentations = extract(
            &page("https://www.instagram.com/reels/example/"),
            &page("https://www.instagram.com/api/graphql"),
            &document,
        )
        .expect("Instagram media is extracted");

        assert_eq!(presentations.len(), 1);
        assert_eq!(
            presentations[0].url().as_str(),
            "https://cdn.example/high.mp4"
        );
        assert_eq!(presentations[0].variants().len(), 2);
        assert_eq!(presentations[0].variants()[1].label(), "640x360");
        assert_eq!(presentations[0].title(), Some("A reel"));
    }

    #[test]
    fn extracts_facebook_progressive_fallbacks_without_duplicates() {
        let document = json!({
            "video": {
                "video_id": "8",
                "browser_native_hd_url": "https://cdn.example/hd.mp4",
                "playable_url_quality_hd": "https://cdn.example/hd.mp4",
                "browser_native_sd_url": "https://cdn.example/sd.mp4"
            }
        });

        let presentations = extract(
            &page("https://www.facebook.com/watch/?v=8"),
            &page("https://www.facebook.com/api/graphql/"),
            &document,
        )
        .expect("Facebook media is extracted");

        assert_eq!(presentations.len(), 1);
        assert_eq!(presentations[0].variants().len(), 2);
        assert_eq!(presentations[0].variants()[0].label(), "HD");
    }

    #[test]
    fn prefers_a_youtube_hls_manifest_over_direct_formats() {
        let document = json!({
            "videoDetails": {
                "videoId": "video-9",
                "title": "A film"
            },
            "streamingData": {
                "hlsManifestUrl": "https://cdn.example/master.m3u8",
                "formats": [
                    {"url": "https://cdn.example/video.mp4", "width": 1280, "height": 720}
                ]
            }
        });

        let presentations = extract(
            &page("https://www.youtube.com/watch?v=video-9"),
            &page("https://www.youtube.com/youtubei/v1/player"),
            &document,
        )
        .expect("YouTube media is extracted");

        assert_eq!(presentations.len(), 1);
        assert_eq!(
            presentations[0].url().as_str(),
            "https://cdn.example/master.m3u8"
        );
        assert_eq!(presentations[0].title(), Some("A film"));
        assert!(presentations[0].variants().is_empty());
    }

    #[test]
    fn refuses_an_unsupported_page() {
        assert_eq!(
            extract(
                &page("https://example.com/watch"),
                &page("https://example.com/data.json"),
                &json!({"video_url": "https://cdn.example/movie.mp4"})
            )
            .expect_err("the page needs a supported extractor"),
            Refusal::UnsupportedSource
        );
    }
}
