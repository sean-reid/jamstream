//! Provider-agnostic cloud core: the Provider trait, mock and contract
//! suite, latency probing, region ranking, cost preview, orphan sweeper,
//! and cloud-init rendering. Concrete AWS, DigitalOcean, and GCP
//! implementations plug in behind the Provider trait.
//!
//! # Session recording
//!
//! The storage half of session recording lives here too, behind
//! [`ObjectStore`]: uploads go to the *host's own* bucket, so this crate owns
//! the S3/Spaces/GCS clients, the multipart upload with its abort guarantee
//! ([`storage`]), the retention rules that make "delete after 30 days" a real
//! server-side lifecycle rule ([`retention`]), and the up-front cost estimate
//! ([`recording`]). The encoder that produces the FLAC, the names the takes
//! get, and the ordering against VM teardown belong to the server.
//!
//! The storage-side flow, as the server drives it:
//!
//! ```text
//! let store: Arc<dyn ObjectStore> = storage.object_store()?;                      // per the host's provider
//! store.set_retention(&bucket, &session_prefix(&session_id), retention).await?;    // before recording
//! let sink = ObjectSink::open(store, bucket, key, FLAC_CONTENT_TYPE, markers);     // one per take, key named
//! sink.write(chunk).await?;                                                        // by the recorder
//! sink.finish().await?;                                                            // then tear down
//! ```

pub mod artifact;
pub mod cloudinit;
/// The provider contract suite. Test-only: see the `testing` feature.
#[cfg(any(test, feature = "testing"))]
pub mod contract;
pub mod cost;
pub mod date;
pub mod http;
/// The scriptable provider double. Test-only: see the `testing` feature.
#[cfg(any(test, feature = "testing"))]
pub mod mock;
pub mod private;
pub mod probe;
pub mod provider;
pub mod providers;
pub mod recording;
pub mod regions;
pub mod retention;
pub mod solver;
pub mod storage;
pub mod sweeper;
pub mod types;

pub use artifact::{
    PinnedServerArtifact, PinnedServerArtifacts, ServerArch, pinned, validate_pair,
};
pub use cloudinit::{
    BootConfig, MediaArtifact, MediaArtifacts, MediaTool, RecordingStorage, SelfDestruct,
    StorageCredential,
};
#[cfg(any(test, feature = "testing"))]
pub use contract::assert_provider_contract;
pub use cost::{CostPreview, LineItem};
pub use date::civil_from_days;
#[cfg(any(test, feature = "testing"))]
pub use mock::MockProvider;
pub use probe::{ProbeTarget, probe_all, probe_catalog};
pub use provider::{Provider, ProviderError, Result, Sleeper, TokioSleeper, WaitOpts};
pub use recording::{
    BitDepth, EgressQuote, RecordingEstimate, RecordingPlan, StoragePrice, storage_price,
};
pub use regions::{RegionTable, priced_regions};
pub use retention::{Retention, RetentionEnforcement};
pub use solver::{MemberId, ProbeMatrix, RegionScore, rank};
#[cfg(feature = "gcp")]
pub use storage::GcsStore;
pub use storage::{
    BytesSource, ChunkSink, FLAC_CONTENT_TYPE, JSON_CONTENT_TYPE, ObjectMeta, ObjectSink,
    ObjectStore, PartSource, ReadSource, S3Store, session_prefix, windows_hazard,
};
#[cfg(any(test, feature = "testing"))]
pub use storage::{MockStore, assert_object_store_contract};
pub use sweeper::{SweepFilter, SweepReport, sweep};
pub use types::{
    ANY_IPV4, ANY_IPV6, DEFAULT_SESSION_PORT, HANDSHAKE_CAP, IP_POLL_PERIOD, IP_WAIT_CAP,
    IngressRule, Instance, InstanceClass, LaunchSpec, Price, ProviderKind, Region, RegionId,
    SESSION_TAG_KEY, format_microusd, session_id_from_tags, session_tag,
};
