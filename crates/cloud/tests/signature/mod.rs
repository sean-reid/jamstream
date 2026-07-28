//! Recomputes a SigV4 signature from the request a wiremock fake received.
//!
//! Every fake in this crate used to check the shape of the `Authorization`
//! header: that it starts with `AWS4-HMAC-SHA256 Credential=`, that a couple of
//! headers exist. A store that signed a canonical request nobody sent, or
//! signed everything with a constant, passed all of it, while the comment on
//! the check claimed the fake was testing the signer.
//!
//! So this rebuilds the canonical request out of what arrived on the wire (the
//! method, the path, the query re-canonicalized by this file's own encoder, the
//! values of exactly the headers the signature claims to cover, and the hash of
//! the body as received), signs it, and compares the header byte for byte. A
//! signature over anything other than the request that turned up fails.
//!
//! The SHA-256 and HMAC underneath come from the crate being tested rather than
//! a second copy, because those are pinned by FIPS 180-4, RFC 4231 and AWS's
//! own `get-vanilla` vector in `providers::aws`. What has no vector, and what
//! this file exists for, is the assembly: which bytes go into the canonical
//! request, and whether they are the bytes that were sent.
#![allow(dead_code)]

use jamstream_cloud::providers::aws::sigv4;
use wiremock::Request;

/// Who the request should have been signed by and for.
pub struct Signer {
    pub access_key_id: &'static str,
    pub secret_access_key: &'static str,
    pub region: &'static str,
    /// `s3`, `ec2` or `ssm`.
    pub service: &'static str,
}

impl Signer {
    /// The AWS test key pair the storage tests use.
    pub fn s3(
        access_key_id: &'static str,
        secret_access_key: &'static str,
        region: &'static str,
    ) -> Signer {
        Signer {
            access_key_id,
            secret_access_key,
            region,
            service: "s3",
        }
    }
}

/// Why a received request is not correctly signed, or Ok when the signature
/// recomputed from the wire matches the one that arrived.
pub fn verify(request: &Request, signer: &Signer) -> Result<(), String> {
    let auth =
        header(request, "authorization").ok_or_else(|| "no authorization header".to_owned())?;
    let parts = Parsed::from_header(&auth)?;

    // The identity, which a shape check never looked at: a request signed by
    // some other key, for some other service or region, is not this store's.
    let expected_scope = format!(
        "{}/{}/{}/aws4_request",
        parts.scope_date, signer.region, signer.service
    );
    if parts.access_key_id != signer.access_key_id {
        return Err(format!(
            "signed with {}, expected {}",
            parts.access_key_id, signer.access_key_id
        ));
    }
    if parts.scope != expected_scope {
        return Err(format!(
            "credential scope is {}, expected {expected_scope}",
            parts.scope
        ));
    }

    // The header set has to be lowercase, sorted, unique, and cover host and
    // the date, or the far end cannot rebuild the canonical request at all.
    let names = &parts.signed_headers;
    if names.windows(2).any(|w| w[0] >= w[1]) {
        return Err(format!("SignedHeaders is not sorted and unique: {names:?}"));
    }
    if names.iter().any(|n| *n != n.to_ascii_lowercase()) {
        return Err(format!("SignedHeaders is not lowercase: {names:?}"));
    }
    for required in ["host", "x-amz-date"] {
        if !names.iter().any(|n| n == required) {
            return Err(format!("{required} is not signed: {names:?}"));
        }
    }

    // Every signed header's value, taken from the request as it arrived. A
    // header that was signed and not sent shows up here as a missing value.
    // The Host header as it went out, not the url wiremock reconstructs: that
    // one carries a placeholder authority, and `host` is the header whose value
    // the signature covers.
    let host = header(request, "host").ok_or_else(|| "no host header".to_owned())?;
    let mut values: Vec<(String, String)> = Vec::new();
    for name in names {
        let value = if name == "host" {
            host.clone()
        } else {
            header(request, name).ok_or_else(|| format!("{name} is signed but was not sent"))?
        };
        values.push((name.clone(), value));
    }

    // The timestamp the signature covers, and the date its scope repeats.
    let amz_date =
        header(request, "x-amz-date").ok_or_else(|| "no x-amz-date header".to_owned())?;
    if amz_date.len() < 8 || amz_date[..8] != parts.scope_date {
        return Err(format!(
            "x-amz-date {amz_date} does not match the credential scope date {}",
            parts.scope_date
        ));
    }

    // The payload hash. S3 sends it in a signed header, and it has to be the
    // hash of the body that arrived; other services hash the body directly.
    let body_hash = if request.body.is_empty() {
        sigv4::EMPTY_PAYLOAD_SHA256.to_owned()
    } else {
        sigv4::hex_sha256(&request.body)
    };
    if let Some(sent) = header(request, "x-amz-content-sha256") {
        if sent != body_hash {
            return Err(format!(
                "x-amz-content-sha256 is {sent}, but the body that arrived hashes to {body_hash}"
            ));
        }
    }

    // The canonical query string, rebuilt by this file's own encoder from the
    // decoded pairs. S3 exercises query canonicalization and AWS's published
    // vector does not, so this is the only check on it.
    let canonical_query = canonical_query(request);
    if let Some(sent) = request.url.query() {
        if sent != canonical_query {
            return Err(format!(
                "query string {sent} is not in canonical form ({canonical_query})"
            ));
        }
    }

    let refs: Vec<(&str, &str)> = values
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let expected = sigv4::authorization_for(&sigv4::SignedRequest {
        access_key_id: signer.access_key_id,
        secret_access_key: signer.secret_access_key,
        region: signer.region,
        service: signer.service,
        method: request.method.as_str(),
        canonical_uri: request.url.path(),
        query: &canonical_query,
        amz_date: &amz_date,
        headers: &refs,
        payload_sha256: &body_hash,
    });
    if expected != auth {
        return Err(format!(
            "the signature does not cover the request that arrived.\n  sent:     {auth}\n  \
             recomputed: {expected}\n  from: {} {}{}",
            request.method.as_str(),
            request.url.path(),
            request
                .url
                .query()
                .map(|q| format!("?{q}"))
                .unwrap_or_default()
        ));
    }
    Ok(())
}

/// Panics with the reason when `request` is not signed as `signer` would have.
pub fn assert_signed(request: &Request, signer: &Signer) {
    if let Err(why) = verify(request, signer) {
        panic!("{why}");
    }
}

/// The pieces of an `Authorization` header value.
struct Parsed {
    access_key_id: String,
    scope: String,
    scope_date: String,
    signed_headers: Vec<String>,
}

impl Parsed {
    fn from_header(auth: &str) -> Result<Parsed, String> {
        let rest = auth
            .strip_prefix("AWS4-HMAC-SHA256 Credential=")
            .ok_or_else(|| format!("not a SigV4 authorization: {auth}"))?;
        let (credential, rest) = rest
            .split_once(", SignedHeaders=")
            .ok_or_else(|| format!("no SignedHeaders in {auth}"))?;
        let (signed, _signature) = rest
            .split_once(", Signature=")
            .ok_or_else(|| format!("no Signature in {auth}"))?;
        let (access_key_id, scope) = credential
            .split_once('/')
            .ok_or_else(|| format!("no credential scope in {auth}"))?;
        let date = scope
            .split('/')
            .next()
            .ok_or_else(|| format!("no date in scope {scope}"))?;
        Ok(Parsed {
            access_key_id: access_key_id.to_owned(),
            scope: scope.to_owned(),
            scope_date: date.to_owned(),
            signed_headers: signed.split(';').map(str::to_owned).collect(),
        })
    }
}

fn header(request: &Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// SigV4's canonical query string, built here rather than taken from the
/// request: pairs sorted by encoded key then encoded value, both percent-encoded
/// with AWS's unreserved set, valueless keys rendered as `key=`.
fn canonical_query(request: &Request) -> String {
    let mut pairs: Vec<(String, String)> = request
        .url
        .query_pairs()
        .map(|(k, v)| (aws_encode(&k), aws_encode(&v)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encoding by AWS's rule: everything but `A-Za-z0-9-_.~`, one byte at
/// a time, uppercase hex. Written here so the store's own encoder is checked
/// against something rather than against itself.
fn aws_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
