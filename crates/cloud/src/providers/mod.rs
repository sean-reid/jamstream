//! Concrete Provider implementations.

pub mod aws;
pub mod digitalocean;
// GCP launching signs a service account JWT with RSA, which is the only
// asymmetric crypto in this crate and the only reason aws-lc-sys is here.
// jamstreamd records to GCS through the S3 interop endpoint instead, so it
// builds without this feature and links no aws-lc: that C does not link
// against musl for aarch64.
#[cfg(feature = "gcp")]
pub mod gcp;
#[cfg(feature = "gcp")]
pub mod gcp_auth;
pub mod local;
