use gunda_core::download::{DownloadDestination, DownloadId, ResolvedDestination};
use percent_encoding::percent_decode_str;
use url::Url;

use crate::FinalizeError;
use crate::finalize::validate_filename;

pub(crate) fn plan_destination(
    id: DownloadId,
    url: &Url,
    destination: &DownloadDestination,
) -> Result<(DownloadDestination, ResolvedDestination), FinalizeError> {
    let filename = match destination.preferred_filename() {
        Some(filename) => {
            validate_filename(filename)?;
            filename.to_owned()
        }
        None => filename_from_url(url).unwrap_or_else(|| format!("download-{}.bin", id.value())),
    };

    let resolved = ResolvedDestination::new(destination.directory().join(&filename));

    let publication = DownloadDestination::new(
        destination.directory().to_path_buf(),
        Some(filename),
        destination.conflict_policy(),
    );

    Ok((publication, resolved))
}

fn filename_from_url(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.next_back()?;

    // Decode only the final segment so encoded separators cannot create a path.
    let filename = percent_decode_str(segment).decode_utf8().ok()?;

    validate_filename(&filename).ok()?;

    Some(filename.into_owned())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gunda_core::download::{DownloadDestination, DownloadId, FileConflictPolicy};
    use url::Url;

    use super::plan_destination;
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
}
