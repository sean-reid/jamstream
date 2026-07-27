//! Provider-agnostic cloud core: the Provider trait, mock and contract
//! suite, latency probing, region ranking, cost preview, orphan sweeper,
//! and cloud-init rendering. Concrete AWS, DigitalOcean, and GCP
//! implementations plug in behind the Provider trait.

pub mod artifact;
pub mod cloudinit;
pub mod contract;
pub mod cost;
pub mod http;
pub mod mock;
pub mod probe;
pub mod provider;
pub mod providers;
pub mod solver;
pub mod sweeper;
pub mod types;

pub use artifact::{PinnedServerArtifact, pinned};
pub use cloudinit::{BootConfig, SelfDestruct};
pub use contract::assert_provider_contract;
pub use cost::{CostPreview, LineItem};
pub use mock::MockProvider;
pub use probe::{ProbeTarget, probe_all, probe_catalog};
pub use provider::{Provider, ProviderError, Result, Sleeper, TokioSleeper, WaitOpts};
pub use solver::{MemberId, ProbeMatrix, RegionScore, rank};
pub use sweeper::{SweepFilter, SweepReport, sweep};
pub use types::{
    Instance, InstanceClass, LaunchSpec, Price, ProviderKind, Region, RegionId, SESSION_TAG_KEY,
    format_microusd, session_id_from_tags, session_tag,
};
