//! Concrete Provider implementations.

pub mod aws;
pub mod digitalocean;
// GCP launching signs a service account JWT with RSA, the only asymmetric
// crypto in this crate. jamstreamd records to GCS through the S3 interop
// endpoint instead and never launches a VM, so it builds without this
// feature and carries neither module.
#[cfg(feature = "gcp")]
pub mod gcp;
#[cfg(feature = "gcp")]
pub mod gcp_auth;
pub mod local;
