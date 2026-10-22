use gunda_core::download::{DownloadDestination, DownloadId, ResolvedDestination};
use percent_encoding::percent_decode_str;
use url::Url;

use crate::FinalizeError;
use crate::finalize::validate_filename;

/// Chooses the publication name for a download.
///
/// This is shared policy, not an HTTP detail: every engine must derive the same
/// final filename for the same job, so protocol engines call it directly instead
/// of reimplementing the naming rules.
pub fn plan_destination(
    id: DownloadId,
    url: &Url,
    destination: &DownloadDestination,
) -> Result<(DownloadDestination, ResolvedDestination), FinalizeError> {
    let filename = match destination.preferred_filename() {
        Some(filename) => {
            validate_filename(filename)?;
            filename.to_owned()
        }
        None => url_filename(url).unwrap_or_else(|| format!("download-{}.bin", id.value())),
    };

    let resolved = ResolvedDestination::new(destination.directory().join(&filename));

    let publication = DownloadDestination::new(
        destination.directory().to_path_buf(),
        Some(filename),
        destination.conflict_policy(),
    );

    Ok((publication, resolved))
}

/// The filename a URL implies, when it implies a usable one.
///
/// Shared because a download does not always publish the resource it addressed:
/// a stream is downloaded from segments and published under the name of the
/// presentation, so the caller needs the same validated stem the namer would
/// have used.
pub fn url_filename(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.next_back()?;

    // Decode only the final segment so encoded separators cannot create a path.
    let filename = percent_decode_str(segment).decode_utf8().ok()?;

    validate_filename(&filename).ok()?;

    Some(filename.into_owned())
}

/// Builds a validated filename from a page title.
///
/// Browser pages often expose an opaque manifest name while the page title
/// describes the presentation. Unsafe filesystem characters are replaced and
/// the title is bounded before the normal filename validation rules are applied.
#[must_use]
pub fn title_filename(title: &str, extension: Option<&str>) -> Option<String> {
    let title = strip_page_title_annotations(title.trim());
    let mut stem = String::with_capacity(title.len().min(160));
    let mut previous_space = false;

    for character in title.chars() {
        let character = if character.is_control()
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '~'
            ) {
            ' '
        } else {
            character
        };

        if character.is_whitespace() {
            if !previous_space {
                stem.push(' ');
            }
            previous_space = true;
        } else {
            stem.push(character);
            previous_space = false;
        }
    }

    let stem = stem.trim_matches([' ', '.']);
    if stem.is_empty() {
        return None;
    }

    let mut end = stem.len().min(160);
    while end > 0 && !stem.is_char_boundary(end) {
        end -= 1;
    }
    let stem = stem[..end].trim_matches([' ', '.']);
    if stem.is_empty() {
        return None;
    }

    let extension = extension.filter(|extension| {
        !extension.is_empty()
            && extension.len() <= 10
            && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    });
    let filename = match extension {
        Some(extension) => format!("{stem}.{extension}"),
        None => stem.to_owned(),
    };

    if validate_filename(&filename).is_ok() {
        return Some(filename);
    }

    let filename = match extension {
        Some(extension) => format!("download {stem}.{extension}"),
        None => format!("download {stem}"),
    };
    validate_filename(&filename).ok().map(|()| filename)
}

fn strip_page_title_annotations(mut title: &str) -> &str {
    if let Some(rest) = title.strip_prefix('(')
        && let Some(end) = rest.find(')')
        && end <= 3
        && rest[..end].bytes().all(|byte| byte.is_ascii_digit())
    {
        title = rest[end + 1..].trim_start();
    }

    let lowercase = title.to_ascii_lowercase();
    for suffix in [
        " - youtube music",
        " | youtube music",
        " - youtube",
        " | youtube",
    ] {
        if lowercase.ends_with(suffix) {
            return title[..title.len() - suffix.len()].trim_end();
        }
    }

    title
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gunda_core::download::{DownloadDestination, DownloadId, FileConflictPolicy};
    use url::Url;

    use super::{plan_destination, title_filename};
    use crate::FinalizeError;

    #[test]
    fn preferred_filename_takes_precedence() {
        let directory = PathBuf::from("downloads");
        let destination = DownloadDestination::new(
            directory.clone(),
            Some("chosen.pdf".to_owned()),
            FileConflictPolicy::Rename,
        );

        let (publication, resolved) = plan_destination(
            DownloadId::new(4).expect("ID must be valid"),
            &Url::parse("https://example.com/remote.pdf").expect("URL must be valid"),
            &destination,
        )
        .expect("destination must resolve");

        assert_eq!(publication.preferred_filename(), Some("chosen.pdf"));
        assert_eq!(publication.conflict_policy(), FileConflictPolicy::Rename);
        assert_eq!(
            resolved.final_path(),
            directory.join("chosen.pdf").as_path(),
        );
    }

    #[test]
    fn invalid_preferred_filename_is_not_silently_replaced() {
        let destination = DownloadDestination::new(
            PathBuf::from("downloads"),
            Some("../chosen.pdf".to_owned()),
            FileConflictPolicy::Fail,
        );

        let result = plan_destination(
            DownloadId::new(4).expect("ID must be valid"),
            &Url::parse("https://example.com/safe.pdf").expect("URL must be valid"),
            &destination,
        );

        assert!(matches!(result, Err(FinalizeError::InvalidFilename)));
    }

    #[test]
    fn url_filename_is_decoded_once_and_validated() {
        let cases = [
            ("https://example.com/files/manual.pdf", "manual.pdf"),
            (
                "https://example.com/files/my%20manual.pdf?token=secret#page",
                "my manual.pdf",
            ),
            ("https://example.com/files/caf%C3%A9.pdf", "café.pdf"),
            ("https://example.com/files/foo%252fbar.txt", "foo%2fbar.txt"),
            ("https://example.com/files/%2e%2e/secret.txt", "secret.txt"),
            ("https://example.com/files/foo%2fbar.txt", "download-4.bin"),
            ("https://example.com/files/foo%5cbar.txt", "download-4.bin"),
            (
                "https://example.com/files/%2e%2e%2fsecret.txt",
                "download-4.bin",
            ),
            ("https://example.com/files/%00name.txt", "download-4.bin"),
            ("https://example.com/files/%FF.txt", "download-4.bin"),
            ("https://example.com/files/CON.txt", "download-4.bin"),
            ("https://example.com/files/name%3Astream", "download-4.bin"),
            ("https://example.com/files/name%2E", "download-4.bin"),
            ("https://example.com/files/.gunda-4.part", "download-4.bin"),
            ("https://example.com/files/", "download-4.bin"),
        ];

        let directory = PathBuf::from("downloads");
        let destination =
            DownloadDestination::new(directory.clone(), None, FileConflictPolicy::Fail);
        let id = DownloadId::new(4).expect("ID must be valid");

        for (raw_url, expected) in cases {
            let url = Url::parse(raw_url).expect("URL must be valid");

            let (publication, resolved) =
                plan_destination(id, &url, &destination).expect("destination must resolve");

            assert_eq!(
                publication.preferred_filename(),
                Some(expected),
                "unexpected filename for {raw_url}",
            );

            assert_eq!(
                resolved.final_path(),
                directory.join(expected).as_path(),
                "unexpected destination for {raw_url}",
            );
        }
    }

    #[test]
    fn page_titles_become_valid_media_filenames() {
        assert_eq!(
            title_filename("Video: part 1/2?", Some("mp4")).as_deref(),
            Some("Video part 1 2.mp4")
        );
        assert_eq!(
            title_filename("CON", Some("mp4")).as_deref(),
            Some("download CON.mp4")
        );
        assert_eq!(
            title_filename("(12) Arcane - Remember Me | YouTube", Some("mp4")).as_deref(),
            Some("Arcane - Remember Me.mp4")
        );
        assert_eq!(
            title_filename("  ...  ", Some("mp4")),
            None,
            "a title with no usable text must fall back to the source name"
        );

        let long = title_filename(&"á".repeat(200), Some("mp4")).expect("a title is usable");
        assert!(long.len() <= 200);
        assert!(long.ends_with(".mp4"));
    }
}
