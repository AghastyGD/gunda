use url::Url;

/// Client that originally submitted a download.
#[derive(Clone, PartialEq, Eq)]
pub enum DownloadOrigin {
    Desktop,

    Cli,

    Browser {
        page_url: Url,
        page_title: Option<String>,
    },
}
