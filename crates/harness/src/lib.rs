//! Deterministic simulation substrate for JamStream: a hand-advanced virtual
//! clock and a seeded network simulator with per-link latency, jitter, loss,
//! reordering, and duplication. The same seed and call sequence reproduce the
//! same deliveries exactly, so networked bugs replay in milliseconds on a
//! laptop instead of once a month on stage.

pub mod clock;
pub mod net;
pub mod profiles;
pub mod scenario;

pub use clock::{SkewedClock, VirtualClock};
pub use net::{Delivery, EndpointId, LinkStats, Profile, SimNet};
pub use scenario::{Scenario, ScenarioBuilder, Source, Traffic};
