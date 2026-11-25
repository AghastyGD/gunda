//! Bounded extraction of media presentations from captured page JSON.
//!
//! Site adapters only recognise the shape a site uses to describe media. URL
//! validation, variant limits, ordering, and candidate creation stay in the
//! daemon so browser-side code cannot inject unvalidated downloads.

use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::{Map, Value};
use url::Url;

use crate::protocol::Refusal;

/// Largest source value depth inspected by one extraction.
const MAX_DEPTH: usize = 32;

/// Largest source value count inspected by one extraction.
const MAX_NODES: usize = 200_000;

/// Largest number of presentations returned by one source.
const MAX_PRESENTATIONS: usize = 16;

/// Largest number of progressive variants retained for one presentation.
const MAX_VARIANTS: usize = 24;

/// Largest media URL accepted from a source.
const MAX_URL_BYTES: usize = 8 * 1024;

/// Largest title retained for one presentation.
const MAX_TITLE_CHARS: usize = 200;

/// Largest site media identifier retained for one presentation.
const MAX_MEDIA_ID_BYTES: usize = 64;

/// One directly downloadable quality of a presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaVariant {
    url: Url,
    content_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    bitrate: Option<u64>,
    size: Option<u64>,
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
    pub const fn size(&self) -> Option<u64> {
        self.size
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
    pub(crate) media_key: Option<String>,
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

    /// The site's identifier for the post this media belongs to.
    #[must_use]
    pub fn media_key(&self) -> Option<&str> {
        self.media_key.as_deref()
    }
}

/// Extracts supported presentations from one bounded JSON response.
///
/// # Errors
///
/// Returns [`Refusal::UnsupportedMedia`] when the document contains no media.
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
    Generic,
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
            Some(Self::Generic)
        }
    }
}

/// What the nearest enclosing objects say about the post being visited.
#[derive(Debug, Clone, Default)]
struct Context {
    media_key: Option<Rc<str>>,
    title: Option<Rc<str>>,
}

struct Collector<'a> {
    platform: Platform,
    source_url: &'a Url,
    presentations: BTreeMap<String, PresentationBuilder>,
    fallback_variants: Vec<(Context, MediaVariant)>,
    context: Context,
    nodes: usize,
}

impl<'a> Collector<'a> {
    fn new(platform: Platform, source_url: &'a Url) -> Self {
        Self {
            platform,
            source_url,
            presentations: BTreeMap::new(),
            fallback_variants: Vec::new(),
            context: Context::default(),
            nodes: 0,
        }
    }

    fn visit(&mut self, document: &Value) {
        let mut pending = vec![(document, 0_usize, Context::default())];

        while let Some((value, depth, context)) = pending.pop() {
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
                        pending.extend(items.iter().map(|item| (item, depth + 1, context.clone())));
                    }
                }
                Value::Object(object) => {
                    self.context = context;
                    self.consider(object);
                    if depth < MAX_DEPTH {
                        let inner = Context {
                            media_key: self
                                .media_key(object)
                                .map(Rc::from)
                                .or_else(|| self.context.media_key.clone()),
                            title: self
                                .post_title(object)
                                .map(Rc::from)
                                .or_else(|| self.context.title.clone()),
                        };
                        pending.extend(
                            object
                                .values()
                                .map(|child| (child, depth.saturating_add(1), inner.clone())),
                        );
                    }
                }
                Value::String(value) => {
                    if let Some(variant) = media_url_variant(value)
                        && self.fallback_variants.len() < MAX_VARIANTS
                        && !self
                            .fallback_variants
                            .iter()
                            .any(|(_, existing)| existing.url == variant.url)
                    {
                        self.fallback_variants.push((context, variant));
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
            Platform::Generic => self.consider_generic(object),
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

        for (name, label, rank) in [
            ("browser_native_hd_url", "HD", 2),
            ("playable_url_quality_hd", "HD", 2),
            ("hd_src", "HD", 2),
            ("browser_native_sd_url", "SD", 1),
            ("playable_url", "SD", 1),
            ("sd_src", "SD", 1),
        ] {
            add_labeled_variant(
                &mut variants,
                object.get(name),
                Some("video/mp4"),
                object,
                label,
                rank,
            );
        }

        if let Some(progressive) = object.get("progressive_urls").and_then(Value::as_array) {
            for item in progressive.iter().filter_map(Value::as_object) {
                let url = item.get("progressive_url").or_else(|| item.get("url"));
                let quality = item
                    .get("metadata")
                    .and_then(Value::as_object)
                    .and_then(|metadata| metadata.get("quality"))
                    .and_then(Value::as_str);
                match quality {
                    Some(quality) if quality.eq_ignore_ascii_case("hd") => {
                        add_labeled_variant(&mut variants, url, Some("video/mp4"), item, "HD", 2);
                    }
                    Some(quality) if quality.eq_ignore_ascii_case("sd") => {
                        add_labeled_variant(&mut variants, url, Some("video/mp4"), item, "SD", 1);
                    }
                    _ => add_variant(&mut variants, url, Some("video/mp4"), item),
                }
            }
        }

        if !variants.is_empty() {
            let title =
                facebook_title(object).or_else(|| self.context.title.as_deref().map(str::to_owned));
            self.add_presentation(object, variants, None, title);
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
        for name in ["formats", "adaptiveFormats"] {
            if let Some(formats) = streaming.get(name).and_then(Value::as_array) {
                for format in formats {
                    if let Some(format) = format.as_object() {
                        add_variant(&mut variants, format.get("url"), None, format);
                    }
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

    fn consider_generic(&mut self, object: &Map<String, Value>) {
        let mut variants = Vec::new();

        for name in [
            "video_url",
            "videoUrl",
            "playback_url",
            "playbackUrl",
            "playable_url",
            "playableUrl",
            "stream_url",
            "streamUrl",
            "manifest_url",
            "manifestUrl",
            "hlsManifestUrl",
            "dashManifestUrl",
            "contentUrl",
            "file",
            "src",
        ] {
            add_variant(&mut variants, object.get(name), None, object);
        }

        for name in [
            "sources",
            "formats",
            "adaptiveFormats",
            "progressive",
            "progressive_urls",
            "video_versions",
            "videos",
            "versions",
        ] {
            if let Some(items) = object.get(name).and_then(Value::as_array) {
                for item in items {
                    if let Some(item) = item.as_object() {
                        add_variant(
                            &mut variants,
                            item.get("url").or_else(|| item.get("src")),
                            None,
                            item,
                        );
                    }
                }
            }
        }

        if let Some(url) = object.get("url") {
            let content_type = string_field(object, &["mimeType", "mime_type", "content_type"]);
            let named_as_media = url
                .as_str()
                .is_some_and(|url| media_path(Url::parse(url).ok().as_ref()));
            if content_type.as_deref().is_some_and(|content_type| {
                content_type.starts_with("video/") || content_type.starts_with("audio/")
            }) || named_as_media
            {
                add_variant(&mut variants, Some(url), content_type.as_deref(), object);
            }
        }

        if !variants.is_empty() {
            self.add_presentation(object, variants, None, None);
        }
    }

    fn add_presentation(
        &mut self,
        object: &Map<String, Value>,
        variants: Vec<MediaVariant>,
        manifest: Option<MediaVariant>,
        title: Option<String>,
    ) {
        // A post's media is often split across nested objects that carry no
        // identifier of their own, so they group under the enclosing post.
        let own_key = self.media_key(object);
        let media_key = own_key
            .clone()
            .or_else(|| self.context.media_key.as_deref().map(str::to_owned));
        let key = own_key
            .or_else(|| presentation_key(object))
            .or_else(|| media_key.clone())
            .or_else(|| {
                variants
                    .first()
                    .map(|variant| variant.url.as_str().to_owned())
            })
            .or_else(|| manifest.as_ref().map(|item| item.url.as_str().to_owned()))
            .unwrap_or_else(|| self.source_url.as_str().to_owned());

        let builder = self.presentations.entry(key).or_default();

        if builder.title.is_none() {
            builder.title = title;
        }
        if builder.media_key.is_none() {
            builder.media_key = media_key;
        }
        if builder.manifest.is_none() {
            builder.manifest = manifest;
        }
        for variant in variants {
            builder.add_variant(variant);
        }
    }

    /// Names the post an object describes, in the form page lookups use.
    fn media_key(&self, object: &Map<String, Value>) -> Option<String> {
        let (site, names, valid): (&str, &[&str], fn(&str) -> bool) = match self.platform {
            Platform::Instagram => ("instagram", &["code", "shortcode"], |id| {
                id.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            }),
            Platform::Facebook => ("facebook", &["video_id", "videoId", "id"], |id| {
                id.bytes().all(|byte| byte.is_ascii_digit())
            }),
            Platform::YouTube | Platform::Generic => return None,
        };

        names
            .iter()
            .filter_map(|name| object.get(*name).and_then(scalar_string))
            .find(|id| !id.is_empty() && id.len() <= MAX_MEDIA_ID_BYTES && valid(id))
            .map(|id| format!("{site}:{id}"))
    }

    /// Text that names a post on sites where media sits below its post.
    fn post_title(&self, object: &Map<String, Value>) -> Option<String> {
        match self.platform {
            Platform::Facebook => facebook_title(object),
            Platform::Instagram | Platform::YouTube | Platform::Generic => None,
        }
    }

    fn finish(self) -> Result<Vec<ExtractedPresentation>, Refusal> {
        let social = matches!(self.platform, Platform::Instagram | Platform::Facebook);
        let mut presentations = self.presentations;

        // Loose media URLs from different posts must not become qualities of
        // one presentation, so social sites group them by post or keep them apart.
        if presentations.is_empty() {
            for (context, variant) in self.fallback_variants {
                let key = match (&context.media_key, social) {
                    (Some(media_key), _) => media_key.to_string(),
                    (None, true) => variant.url.as_str().to_owned(),
                    (None, false) => String::new(),
                };
                if !presentations.contains_key(&key) && presentations.len() >= MAX_PRESENTATIONS {
                    continue;
                }
                let builder = presentations.entry(key).or_default();
                if builder.media_key.is_none() {
                    builder.media_key = context.media_key.as_deref().map(str::to_owned);
                }
                if builder.title.is_none() {
                    builder.title = context.title.as_deref().map(str::to_owned);
                }
                builder.add_variant(variant);
            }
        }

        let mut presentations = presentations
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
    media_key: Option<String>,
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
                variants: self.variants,
                media_key: self.media_key,
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
            media_key: self.media_key,
        })
    }
}

impl MediaVariant {
    fn enrich(&mut self, other: Self) {
        self.content_type = other.content_type.or(self.content_type.take());
        self.width = other.width.or(self.width);
        self.height = other.height.or(self.height);
        self.bitrate = other.bitrate.or(self.bitrate);
        self.size = other.size.or(self.size);
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
        size: integer_field(metadata, &["contentLength", "filesize", "filesize_approx"]),
        codecs,
        label: string_field(metadata, &["quality_label", "quality"]),
        rank: 0,
    })
}

fn media_url_variant(raw: &str) -> Option<MediaVariant> {
    if raw.len() > MAX_URL_BYTES {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") || !media_path(Some(&url)) {
        return None;
    }

    Some(MediaVariant {
        url,
        content_type: None,
        width: None,
        height: None,
        bitrate: None,
        size: None,
        codecs: None,
        label: None,
        rank: 0,
    })
}

fn media_path(url: Option<&Url>) -> bool {
    let Some(url) = url else {
        return false;
    };
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .is_some_and(|segment| {
            [
                "mp4", "m4v", "webm", "mkv", "mov", "flv", "avi", "wmv", "mpeg", "mpg", "3gp",
                "ogv", "m3u8", "mpd", "mp3", "m4a", "aac", "ogg", "oga", "opus", "flac", "wav",
            ]
            .iter()
            .any(|extension| {
                segment
                    .rsplit_once('.')
                    .is_some_and(|(_, actual)| actual.eq_ignore_ascii_case(extension))
            })
        })
}

fn presentation_key(object: &Map<String, Value>) -> Option<String> {
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

    None
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

/// Reads the text Facebook shows with a post or video.
fn facebook_title(object: &Map<String, Value>) -> Option<String> {
    ["title", "message", "savable_description"]
        .iter()
        .filter_map(|name| object.get(*name))
        .filter_map(|value| {
            value
                .as_str()
                .or_else(|| value.as_object()?.get("text")?.as_str())
        })
        .map(clean_title)
        .find(|title| !title.is_empty())
}

/// Joins words split by line breaks or other control characters.
fn clean_title(title: &str) -> String {
    title
        .split(|character: char| character.is_whitespace() || character.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use url::Url;

    use super::{MediaVariant, extract};
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
    fn extracts_facebook_player_sources_with_hd_first() {
        let document = json!({
            "video": {
                "video_id": "8",
                "hd_src": "https://cdn.example/hd.mp4",
                "sd_src": "https://cdn.example/sd.mp4"
            }
        });

        let presentations = extract(
            &page("https://www.facebook.com/reel/8/"),
            &page("https://www.facebook.com/api/graphql/"),
            &document,
        )
        .expect("Facebook player sources are extracted");

        assert_eq!(presentations.len(), 1);
        assert_eq!(
            presentations[0].url().as_str(),
            "https://cdn.example/hd.mp4"
        );
        assert_eq!(presentations[0].variants().len(), 2);
        assert_eq!(presentations[0].variants()[0].label(), "HD");
    }

    #[test]
    fn an_instagram_presentation_names_its_post() {
        let document = json!({
            "media": {
                "pk": "media-8",
                "code": "ABC_12-x",
                "video_versions": [{ "url": "https://cdn.example/reel.mp4" }],
                "video_dash_manifest": "<?xml version=\"1.0\"?><MPD></MPD>"
            }
        });

        let presentations = extract(
            &page("https://www.instagram.com/"),
            &page("https://www.instagram.com/api/graphql"),
            &document,
        )
        .expect("Instagram media is extracted");

        assert_eq!(presentations.len(), 1);
        assert_eq!(presentations[0].media_key(), Some("instagram:ABC_12-x"));
        assert_eq!(presentations[0].variants().len(), 1);
    }

    #[test]
    fn a_facebook_presentation_names_its_video_but_not_a_text_id() {
        let document = json!([
            { "id": "4211", "browser_native_hd_url": "https://cdn.example/a.mp4" },
            { "id": "VXNlcjox", "playable_url": "https://cdn.example/b.mp4" }
        ]);

        let mut keys = extract(
            &page("https://www.facebook.com/"),
            &page("https://www.facebook.com/api/graphql/"),
            &document,
        )
        .expect("Facebook media is extracted")
        .iter()
        .map(|presentation| presentation.media_key().map(str::to_owned))
        .collect::<Vec<_>>();
        keys.sort();

        assert_eq!(keys, [None, Some(String::from("facebook:4211"))]);
    }

    #[test]
    fn facebook_reels_stay_separate_and_keep_their_video_ids() {
        let reel = |id: &str| {
            json!({
                "id": id,
                "videoDeliveryLegacyFields": {
                    "browser_native_sd_url": format!("https://cdn.example/{id}-sd.mp4")
                },
                "videoDeliveryResponseFragment": {
                    "videoDeliveryResponseResult": {
                        "progressive_urls": [
                            {
                                "progressive_url": format!("https://cdn.example/{id}-hd.mp4"),
                                "metadata": { "quality": "HD" }
                            },
                            {
                                "progressive_url": format!("https://cdn.example/{id}-sd.mp4"),
                                "metadata": { "quality": "SD" }
                            }
                        ]
                    }
                }
            })
        };
        let document = json!({ "data": { "reels": [reel("41"), reel("42")] } });

        let presentations = extract(
            &page("https://www.facebook.com/reel/41"),
            &page("https://www.facebook.com/api/graphql/"),
            &document,
        )
        .expect("Facebook reels are extracted");

        assert_eq!(presentations.len(), 2);
        for presentation in &presentations {
            let id = presentation
                .media_key()
                .and_then(|key| key.strip_prefix("facebook:"))
                .expect("each reel names its video");
            assert_eq!(
                presentation.url().as_str(),
                format!("https://cdn.example/{id}-hd.mp4")
            );
            let mut labels = presentation
                .variants()
                .iter()
                .map(MediaVariant::label)
                .collect::<Vec<_>>();
            labels.sort();
            assert_eq!(labels, ["HD", "SD"]);
        }
    }

    #[test]
    fn a_facebook_reel_takes_its_title_from_the_enclosing_post() {
        let document = json!({
            "story": {
                "message": { "text": "A cat that\nlikes boxes" },
                "attachments": [{
                    "media": {
                        "id": "4211",
                        "videoDeliveryLegacyFields": {
                            "browser_native_hd_url": "https://cdn.example/4211-hd.mp4"
                        }
                    }
                }]
            }
        });

        let presentations = extract(
            &page("https://www.facebook.com/reel/4211"),
            &page("https://www.facebook.com/api/graphql/"),
            &document,
        )
        .expect("the reel is extracted");

        assert_eq!(presentations.len(), 1);
        assert_eq!(presentations[0].title(), Some("A cat that likes boxes"));
        assert_eq!(presentations[0].media_key(), Some("facebook:4211"));
    }

    #[test]
    fn loose_facebook_media_urls_from_different_videos_are_not_merged() {
        let document = json!([
            { "id": "41", "node": { "attachment": "https://cdn.example/41.mp4" } },
            { "id": "42", "node": { "attachment": "https://cdn.example/42.mp4" } }
        ]);

        let presentations = extract(
            &page("https://www.facebook.com/reel/41"),
            &page("https://www.facebook.com/api/graphql/"),
            &document,
        )
        .expect("loose media URLs are extracted");

        assert_eq!(presentations.len(), 2);
        assert!(
            presentations
                .iter()
                .all(|presentation| presentation.variants().len() == 1)
        );
    }

    #[test]
    fn finds_a_complete_facebook_media_url_without_a_known_field() {
        let document = json!({
            "node": {
                "id": "video-11",
                "playback_url": "https://cdn.example/fallback.mp4"
            }
        });

        let presentations = extract(
            &page("https://www.facebook.com/watch/?v=11"),
            &page("https://www.facebook.com/api/graphql/"),
            &document,
        )
        .expect("a generic Facebook field is extracted");

        assert_eq!(
            presentations[0].url().as_str(),
            "https://cdn.example/fallback.mp4"
        );
    }

    #[test]
    fn finds_a_complete_instagram_media_url_without_a_known_field() {
        let document = json!({
            "media": {
                "pk": "media-9",
                "playback_url": "https://cdn.example/fallback.mp4"
            }
        });

        let presentations = extract(
            &page("https://www.instagram.com/reel/example/"),
            &page("https://www.instagram.com/api/graphql"),
            &document,
        )
        .expect("a generic Instagram field is extracted");

        assert_eq!(
            presentations[0].url().as_str(),
            "https://cdn.example/fallback.mp4"
        );
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
                ],
                "adaptiveFormats": [
                    {
                        "url": "https://cdn.example/video-only.mp4",
                        "width": 1920,
                        "height": 1080,
                        "contentLength": "345678"
                    }
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
        assert_eq!(
            presentations[0]
                .variants()
                .iter()
                .find(|variant| variant.resolution() == Some((1920, 1080)))
                .and_then(MediaVariant::size),
            Some(345_678)
        );
    }

    #[test]
    fn extracts_media_from_an_unrecognised_site() {
        let presentations = extract(
            &page("https://example.com/watch"),
            &page("https://example.com/data.json"),
            &json!({
                "player": {
                    "id": "movie-1",
                    "video_url": "https://cdn.example/movie.mp4",
                    "width": 1920,
                    "height": 1080
                }
            }),
        )
        .expect("generic media is extracted");

        assert_eq!(presentations.len(), 1);
        assert_eq!(
            presentations[0].url().as_str(),
            "https://cdn.example/movie.mp4"
        );
        assert_eq!(
            presentations[0].variants()[0].resolution(),
            Some((1920, 1080))
        );
    }

    #[test]
    fn falls_back_to_media_urls_in_unstructured_json() {
        let presentations = extract(
            &page("https://example.com/watch"),
            &page("https://example.com/data.json"),
            &json!({
                "payload": [
                    "metadata",
                    "https://cdn.example/fallback.webm"
                ]
            }),
        )
        .expect("a full media URL is enough");

        assert_eq!(
            presentations[0].url().as_str(),
            "https://cdn.example/fallback.webm"
        );
    }
}
