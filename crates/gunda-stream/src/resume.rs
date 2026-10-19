use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use gunda_core::application::RepositoryError;
use gunda_core::download::DownloadId;
use url::Url;

use crate::plan::{StreamKind, TrackPlan};

pub const FINGERPRINT_LEN: usize = 16;

/// Stable identity of the ordered resources a resume checkpoint belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanFingerprint([u8; FINGERPRINT_LEN]);

impl PlanFingerprint {
    /// Hashes the plan structure while ignoring rotating signature parameters.
    ///
    /// Signed segment URLs commonly rotate `token`, `sig`, `expires`, and
    /// cloud-provider signature parameters between attempts while addressing
    /// the same resource. Other query parameters stay part of the identity, so
    /// a query that names a different segment is refused rather than resumed.
    #[must_use]
    pub fn of(plan: &TrackPlan, manifest: &Url) -> Self {
        let mut hash = Fnv128::new();

        hash.write(match plan.kind() {
            StreamKind::Hls => b"hls",
            StreamKind::Dash => b"dash",
        });
        hash.write(b"\0");
        hash_url(&mut hash, manifest);
        hash.write(b"\0");
        hash.write(&(plan.resource_count() as u64).to_le_bytes());

        for resource in plan.resources() {
            hash.write(b"\0");
            hash.write(&[resource.track()]);
            hash_url(&mut hash, resource.url());

            match resource.byte_range() {
                Some(range) => {
                    hash.write(&[1]);
                    hash.write(&range.length().to_le_bytes());
                    hash.write(&range.offset().unwrap_or(0).to_le_bytes());
                }
                None => hash.write(&[0]),
            }
        }

        Self(hash.finish())
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; FINGERPRINT_LEN] {
        &self.0
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, InvalidStreamResumeState> {
        let bytes: [u8; FINGERPRINT_LEN] =
            bytes.try_into().map_err(|_| InvalidStreamResumeState)?;

        Ok(Self(bytes))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackCheckpoint {
    next_resource: usize,
    durable_bytes: u64,
}

impl TrackCheckpoint {
    #[must_use]
    pub const fn new(next_resource: usize, durable_bytes: u64) -> Self {
        Self {
            next_resource,
            durable_bytes,
        }
    }

    #[must_use]
    pub const fn start() -> Self {
        Self::new(0, 0)
    }

    #[must_use]
    pub const fn next_resource(&self) -> usize {
        self.next_resource
    }

    #[must_use]
    pub const fn durable_bytes(&self) -> u64 {
        self.durable_bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamResumeState {
    fingerprint: PlanFingerprint,
    tracks: Vec<TrackCheckpoint>,
}

impl StreamResumeState {
    pub fn new(
        fingerprint: PlanFingerprint,
        tracks: Vec<TrackCheckpoint>,
    ) -> Result<Self, InvalidStreamResumeState> {
        if tracks.is_empty() || tracks.len() > 2 {
            return Err(InvalidStreamResumeState);
        }

        Ok(Self {
            fingerprint,
            tracks,
        })
    }

    #[must_use]
    pub const fn fingerprint(&self) -> PlanFingerprint {
        self.fingerprint
    }

    #[must_use]
    pub fn tracks(&self) -> &[TrackCheckpoint] {
        &self.tracks
    }

    pub fn with_track(
        &self,
        track: usize,
        checkpoint: TrackCheckpoint,
    ) -> Result<Self, InvalidStreamResumeState> {
        let current = self.tracks.get(track).ok_or(InvalidStreamResumeState)?;

        if checkpoint.next_resource() < current.next_resource()
            || checkpoint.durable_bytes() < current.durable_bytes()
        {
            return Err(InvalidStreamResumeState);
        }

        let mut tracks = self.tracks.clone();
        tracks[track] = checkpoint;

        Self::new(self.fingerprint, tracks)
    }

    #[must_use]
    pub fn starting_bytes(&self) -> u64 {
        self.tracks
            .iter()
            .map(TrackCheckpoint::durable_bytes)
            .fold(0_u64, u64::saturating_add)
    }

    #[must_use]
    pub fn is_complete(&self, plan: &TrackPlan) -> bool {
        self.tracks.iter().enumerate().all(|(track, checkpoint)| {
            checkpoint.next_resource() >= plan.resources_of(track as u8).count()
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidStreamResumeState;

impl fmt::Display for InvalidStreamResumeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the stream resume checkpoint is invalid")
    }
}

impl Error for InvalidStreamResumeState {}

pub trait StreamResumeStore: Send + Sync {
    fn initialize(
        &self,
        id: DownloadId,
        fingerprint: PlanFingerprint,
        track_count: usize,
    ) -> impl Future<Output = Result<StreamResumeState, RepositoryError>> + Send;

    fn find(
        &self,
        id: DownloadId,
    ) -> impl Future<Output = Result<Option<StreamResumeState>, RepositoryError>> + Send;

    /// Advances checkpoints after the caller synchronizes every track file.
    fn save_checkpoint(
        &self,
        id: DownloadId,
        state: &StreamResumeState,
    ) -> impl Future<Output = Result<StreamResumeState, RepositoryError>> + Send;

    /// The caller removes staging first.
    fn clear(&self, id: DownloadId) -> impl Future<Output = Result<(), RepositoryError>> + Send;
}

pub(crate) type StoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, RepositoryError>> + Send + 'a>>;

/// Object-safe view of a resume store used by the executor.
pub(crate) trait ResumePersistence: Send + Sync {
    fn initialize_state(
        &self,
        id: DownloadId,
        fingerprint: PlanFingerprint,
        track_count: usize,
    ) -> StoreFuture<'_, StreamResumeState>;

    fn find_state(&self, id: DownloadId) -> StoreFuture<'_, Option<StreamResumeState>>;

    fn save_state(
        &self,
        id: DownloadId,
        state: StreamResumeState,
    ) -> StoreFuture<'_, StreamResumeState>;

    fn clear_state(&self, id: DownloadId) -> StoreFuture<'_, ()>;
}

impl<S: StreamResumeStore> ResumePersistence for S {
    fn initialize_state(
        &self,
        id: DownloadId,
        fingerprint: PlanFingerprint,
        track_count: usize,
    ) -> StoreFuture<'_, StreamResumeState> {
        Box::pin(async move { self.initialize(id, fingerprint, track_count).await })
    }

    fn find_state(&self, id: DownloadId) -> StoreFuture<'_, Option<StreamResumeState>> {
        Box::pin(async move { self.find(id).await })
    }

    fn save_state(
        &self,
        id: DownloadId,
        state: StreamResumeState,
    ) -> StoreFuture<'_, StreamResumeState> {
        Box::pin(async move { self.save_checkpoint(id, &state).await })
    }

    fn clear_state(&self, id: DownloadId) -> StoreFuture<'_, ()> {
        Box::pin(async move { self.clear(id).await })
    }
}

fn hash_url(hash: &mut Fnv128, url: &Url) {
    hash.write(url.scheme().as_bytes());
    hash.write(b"://");
    hash.write(url.host_str().unwrap_or_default().as_bytes());

    if let Some(port) = url.port() {
        hash.write(b":");
        hash.write(port.to_string().as_bytes());
    }

    hash.write(url.path().as_bytes());

    let mut query: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    query.retain(|(name, _)| !is_signing_parameter(name));
    query.sort_unstable();

    for (name, value) in query {
        hash.write(b"?");
        hash.write(name.as_bytes());
        hash.write(b"=");
        hash.write(value.as_bytes());
    }
}

fn is_signing_parameter(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "token"
            | "sig"
            | "signature"
            | "expires"
            | "expiry"
            | "auth"
            | "authorization"
            | "hdnts"
            | "policy"
            | "jwt"
            | "key-pair-id"
            | "x-amz-signature"
            | "x-amz-credential"
            | "x-amz-date"
            | "x-amz-expires"
            | "x-amz-security-token"
            | "x-goog-signature"
            | "x-goog-credential"
            | "x-goog-date"
            | "x-goog-expires"
    )
}

struct Fnv128 {
    first: u64,
    second: u64,
}

impl Fnv128 {
    const fn new() -> Self {
        Self {
            first: 0xcbf2_9ce4_8422_2325,
            second: 0x9ce4_8422_2325_cbf2,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.first ^= u64::from(*byte);
            self.first = self.first.wrapping_mul(0x0000_0100_0000_01b3);

            self.second ^= u64::from(*byte);
            self.second = self
                .second
                .wrapping_mul(0x0000_0100_0000_01b3)
                .rotate_left(23);
        }
    }

    fn finish(&self) -> [u8; FINGERPRINT_LEN] {
        let mut result = [0_u8; FINGERPRINT_LEN];
        result[..8].copy_from_slice(&self.first.to_le_bytes());
        result[8..].copy_from_slice(&self.second.to_le_bytes());
        result
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{PlanFingerprint, StreamResumeState, TrackCheckpoint};
    use crate::plan::{StreamKind, StreamResource, TrackPlan};

    fn plan(paths: &[&str]) -> TrackPlan {
        TrackPlan::new(
            StreamKind::Hls,
            paths
                .iter()
                .map(|path| {
                    StreamResource::new(
                        Url::parse(&format!("https://cdn.example/{path}")).unwrap(),
                        None,
                    )
                })
                .collect(),
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn rotating_query_strings_keep_the_same_fingerprint() {
        let first = plan(&["seg.m4s?token=one", "next.m4s"]);
        let second = plan(&["seg.m4s?token=two", "next.m4s"]);
        let manifest = Url::parse("https://cdn.example/master.m3u8").unwrap();

        assert_eq!(
            PlanFingerprint::of(&first, &manifest),
            PlanFingerprint::of(&second, &manifest)
        );
    }

    #[test]
    fn a_query_that_names_another_segment_changes_the_fingerprint() {
        let first = plan(&["seg.m4s?segment=one"]);
        let second = plan(&["seg.m4s?segment=two"]);
        let manifest = Url::parse("https://cdn.example/master.m3u8").unwrap();

        assert_ne!(
            PlanFingerprint::of(&first, &manifest),
            PlanFingerprint::of(&second, &manifest)
        );
    }

    #[test]
    fn a_different_path_changes_the_fingerprint() {
        let first = plan(&["seg.m4s"]);
        let second = plan(&["other.m4s"]);
        let manifest = Url::parse("https://cdn.example/master.m3u8").unwrap();

        assert_ne!(
            PlanFingerprint::of(&first, &manifest),
            PlanFingerprint::of(&second, &manifest)
        );
    }

    #[test]
    fn checkpoint_state_requires_one_or_two_tracks() {
        let fingerprint = PlanFingerprint::parse(&[0_u8; super::FINGERPRINT_LEN]).unwrap();

        assert!(StreamResumeState::new(fingerprint, Vec::new()).is_err());
        assert!(
            StreamResumeState::new(
                fingerprint,
                vec![TrackCheckpoint::start(), TrackCheckpoint::start()]
            )
            .is_ok()
        );
        assert!(
            StreamResumeState::new(
                fingerprint,
                vec![
                    TrackCheckpoint::start(),
                    TrackCheckpoint::start(),
                    TrackCheckpoint::start()
                ]
            )
            .is_err()
        );
    }

    #[test]
    fn a_complete_checkpoint_knows_the_plan_is_staged() {
        let plan = plan(&["one.m4s", "two.m4s"]);
        let fingerprint = PlanFingerprint::of(
            &plan,
            &Url::parse("https://cdn.example/master.m3u8").unwrap(),
        );
        let state = StreamResumeState::new(fingerprint, vec![TrackCheckpoint::new(2, 10)]).unwrap();

        assert!(state.is_complete(&plan));
        assert_eq!(state.starting_bytes(), 10);
    }
}
