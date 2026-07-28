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
//! ([`recording`]). The writer that produces the WAV, and the ordering
//! against VM teardown, belong to the server.
//!
//! The whole storage-side flow is four calls:
//!
//! ```text
//! let store: Box<dyn ObjectStore> = /* per the host's provider */;
//! store.set_retention(&bucket, &session_prefix(&session_id), retention).await?;   // before recording
//! store.put_stream(&bucket, &mix_key(&session_id), WAV_CONTENT_TYPE, &mut ReadSource::new(file)).await?;
//! store.put_stream(&bucket, &stem_key(&session_id, &member), WAV_CONTENT_TYPE, &mut ReadSource::new(file)).await?;
//! store.put(&bucket, &manifest_key(&session_id), JSON_CONTENT_TYPE, &manifest).await?;   // then tear down
//! ```

pub mod artifact;
pub mod cloudinit;
pub mod contract;
pub mod cost;
pub mod http;
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

pub use artifact::{PinnedServerArtifact, pinned, validate_pair};
pub use cloudinit::{BootConfig, MediaArtifact, MediaArtifacts, MediaTool, SelfDestruct};
pub use contract::assert_provider_contract;
pub use cost::{CostPreview, LineItem};
pub use mock::MockProvider;
pub use probe::{ProbeTarget, probe_all, probe_catalog};
pub use provider::{Provider, ProviderError, Result, Sleeper, TokioSleeper, WaitOpts};
pub use recording::{BitDepth, RecordingEstimate, RecordingPlan, StoragePrice, storage_price};
pub use regions::{RegionTable, priced_regions};
pub use retention::{Retention, RetentionEnforcement};
pub use solver::{MemberId, ProbeMatrix, RegionScore, rank};
pub use storage::{
    BytesSource, GcsStore, JSON_CONTENT_TYPE, MockStore, ObjectMeta, ObjectStore, PartSource,
    ReadSource, S3Store, WAV_CONTENT_TYPE, assert_object_store_contract, manifest_key, mix_key,
    session_prefix, stem_key,
};
pub use sweeper::{SweepFilter, SweepReport, sweep};
pub use types::{
    ANY_IPV4, ANY_IPV6, DEFAULT_SESSION_PORT, IngressRule, Instance, InstanceClass, LaunchSpec,
    Price, ProviderKind, Region, RegionId, SESSION_TAG_KEY, format_microusd, session_id_from_tags,
    session_tag,
};
