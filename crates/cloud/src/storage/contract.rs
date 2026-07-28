//! Generic behavioral suite every [`ObjectStore`] implementation must pass,
//! mirroring [`crate::contract`] for providers. Run it from each
//! implementation's tests; a new backend merges only if this passes
//! unchanged.

use crate::provider::ProviderError;
use crate::retention::{Retention, RetentionEnforcement};
use crate::storage::{
    BytesSource, ChunkSink, FLAC_CONTENT_TYPE, JSON_CONTENT_TYPE, ObjectStore, PartSource,
    session_prefix,
};

/// Panics on the first contract violation.
///
/// `bucket` must exist and hold nothing under
/// [`crate::storage::RECORDING_PREFIX`]; the suite cleans up its objects and
/// asserts that it did. The lifecycle rules for its two session prefixes stay,
/// because the trait has no call that removes one.
pub async fn assert_object_store_contract(store: &dyn ObjectStore, bucket: &str) {
    let session = "contract-session";
    let prefix = session_prefix(session);
    // The three objects a session really writes: a take, a second take, and the
    // launch's write probe. The names are the recorder's shape, spelled out
    // here rather than built by a key helper, because the helpers that used to
    // live in `storage` named objects nothing writes and every store test
    // believed them.
    let mix = format!("{prefix}jamstream-2026-07-25-1030-mix.flac");
    let stem = format!("{prefix}jamstream-2026-07-25-1030-contract-member.flac");
    let probe = format!("{prefix}.jamstream-probe");

    // ---- Absent objects ----

    match store.head(bucket, &mix).await {
        Err(ProviderError::NotFound(_)) => {}
        other => panic!("head of a missing object must be NotFound, got {other:?}"),
    }
    let listed = store
        .list(bucket, &prefix)
        .await
        .expect("list of an empty prefix must succeed");
    assert!(
        listed.is_empty(),
        "the suite requires a clean prefix, found {listed:?}"
    );
    // Idempotent delete: cleanup paths run after partial failures and must
    // not have to know whether the object made it.
    store
        .delete(bucket, &mix)
        .await
        .expect("deleting a missing object must succeed");

    // ---- Single-shot put ----

    let probe_body = br#"{"probe":true}"#;
    let meta = store
        .put(bucket, &probe, JSON_CONTENT_TYPE, probe_body)
        .await
        .expect("put must succeed");
    assert_eq!(meta.key, probe, "put must echo the key it wrote");
    assert_eq!(meta.size, probe_body.len() as u64);

    let head = store.head(bucket, &probe).await.expect("head after put");
    assert_eq!(head.size, probe_body.len() as u64, "head size mismatch");
    if let Some(ct) = &head.content_type {
        assert_eq!(ct, JSON_CONTENT_TYPE, "content type was not preserved");
    }

    // ---- put_stream: one part, so the plain PUT path ----

    // Exactly one full part: the boundary case for the escalation decision,
    // which must still take the single-PUT path because the lookahead read
    // comes back empty.
    let short_len = store.part_size();
    let short: Vec<u8> = (0..short_len).map(|i| (i % 251) as u8).collect();
    let meta = store
        .put_stream(
            bucket,
            &stem,
            FLAC_CONTENT_TYPE,
            &mut BytesSource::new(short.clone()),
        )
        .await
        .expect("single-part put_stream must succeed");
    assert_eq!(meta.size, short.len() as u64);
    assert_eq!(
        store.head(bucket, &stem).await.expect("head stem").size,
        short.len() as u64
    );

    // ---- put_stream: several parts, so the multipart path ----

    // Two and a bit parts, so there is a first part, a middle part, and a
    // short final part.
    let big_len = store.part_size() * 2 + 7;
    let meta = store
        .put_stream(
            bucket,
            &mix,
            FLAC_CONTENT_TYPE,
            &mut CountingSource::new(big_len),
        )
        .await
        .expect("multipart put_stream must succeed");
    assert_eq!(
        meta.size, big_len as u64,
        "the completed object must report the full body size"
    );
    let head = store.head(bucket, &mix).await.expect("head mix");
    assert_eq!(
        head.size, big_len as u64,
        "the stored object is not the size that was uploaded"
    );

    // ---- Listing ----

    let listed = store.list(bucket, &prefix).await.expect("list prefix");
    let keys: Vec<&str> = listed.iter().map(|m| m.key.as_str()).collect();
    for expected in [&mix, &stem, &probe] {
        assert!(
            keys.contains(&expected.as_str()),
            "list under {prefix} missed {expected}: {keys:?}"
        );
    }
    assert!(
        listed.windows(2).all(|w| w[0].key <= w[1].key),
        "list must be sorted by key: {keys:?}"
    );
    // A narrower prefix narrows the listing: the two takes share a timestamp
    // and the probe does not.
    let takes_only = store
        .list(bucket, &format!("{prefix}jamstream-2026-07-25-1030-"))
        .await
        .expect("list one take's objects");
    assert_eq!(
        takes_only.len(),
        2,
        "a narrower prefix must narrow the listing: {takes_only:?}"
    );
    assert!(takes_only.iter().all(|m| m.key != probe));
    let nothing = store
        .list(bucket, "jamstream/recordings/no-such-session/")
        .await
        .expect("list of an unrelated prefix");
    assert!(nothing.is_empty(), "prefix filter leaked: {nothing:?}");

    // ---- Retention ----

    for retention in Retention::ALL {
        let applied = store
            .set_retention(bucket, &prefix, retention)
            .await
            .unwrap_or_else(|e| panic!("set_retention({retention}) failed: {e}"));
        assert_eq!(
            applied.retention(),
            retention,
            "set_retention reported a different choice than it was given"
        );
        assert!(
            !applied.describe().is_empty(),
            "every enforcement outcome needs a line a host can read"
        );
    }

    // A second session in the same bucket must not take the first one's rule
    // with it. Both provider APIs replace the bucket's whole lifecycle
    // document, so this is the whole promise of per-session retention: the
    // document that comes back has to still carry the first session's rule.
    let other = session_prefix("contract-session-two");
    let first = store
        .set_retention(bucket, &prefix, Retention::Days90)
        .await
        .expect("retention for the first session");
    let second = store
        .set_retention(bucket, &other, Retention::Days7)
        .await
        .expect("retention for the second session");
    if let (
        RetentionEnforcement::ServerSide {
            rule_id: first_id, ..
        },
        RetentionEnforcement::ServerSide {
            rule_id: second_id,
            rule: document,
            ..
        },
    ) = (&first, &second)
    {
        assert_ne!(
            first_id, second_id,
            "two sessions must not share one rule id"
        );
        // The prefix rather than the id, because GCS rules carry no id: what
        // has to be true on every provider is that the document the second
        // session left behind still says something about the first session's
        // prefix.
        assert!(
            document.contains(prefix.as_str()),
            "recording a second session deleted the rule for {prefix}: {document}"
        );
        assert!(
            document.contains(other.as_str()),
            "the second session's own rule is not in the document: {document}"
        );
    }

    // ---- Download ----

    // Byte for byte against independently regenerated bytes: a store that
    // echoed back its own idea of the body would pass any weaker check, and a
    // recording that comes back short is the failure that matters most.
    let mut got = Collector::default();
    let meta = store
        .get(bucket, &mix, &mut got)
        .await
        .expect("get of the multipart object must succeed");
    assert_eq!(
        meta.size, big_len as u64,
        "get must report the number of bytes it delivered"
    );
    assert_eq!(
        got.bytes,
        counted_bytes(big_len).await,
        "the downloaded mix is not the body that was uploaded"
    );

    let mut got = Collector::default();
    let meta = store
        .get(bucket, &stem, &mut got)
        .await
        .expect("get of the single-part object must succeed");
    assert_eq!(meta.size, short.len() as u64);
    assert_eq!(
        got.bytes, short,
        "the downloaded stem is not the body that was uploaded"
    );

    let mut nowhere = Collector::default();
    match store
        .get(bucket, &format!("{prefix}absent.flac"), &mut nowhere)
        .await
    {
        Err(ProviderError::NotFound(_)) => {}
        other => panic!("get of a missing object must be NotFound, got {other:?}"),
    }
    assert!(
        nowhere.bytes.is_empty(),
        "a failed get handed bytes to the sink"
    );

    // ---- Cleanup ----

    for key in [&mix, &stem, &probe] {
        store.delete(bucket, key).await.expect("delete");
        match store.head(bucket, key).await {
            Err(ProviderError::NotFound(_)) => {}
            other => panic!("{key} survived delete: {other:?}"),
        }
        // Second delete is a no-op, not an error.
        store.delete(bucket, key).await.expect("double delete");
    }
    let leftovers = store.list(bucket, &prefix).await.expect("final list");
    assert!(
        leftovers.is_empty(),
        "the contract suite must leave nothing behind, found {leftovers:?}"
    );
}

/// Generates `len` deterministic bytes without holding them all in memory
/// twice, so the multipart leg of the suite stays cheap even at a realistic
/// part size.
struct CountingSource {
    remaining: usize,
    produced: usize,
}

impl CountingSource {
    fn new(len: usize) -> Self {
        CountingSource {
            remaining: len,
            produced: 0,
        }
    }
}

#[async_trait::async_trait]
impl PartSource for CountingSource {
    async fn next_part(&mut self, max: usize) -> crate::provider::Result<Vec<u8>> {
        let take = self.remaining.min(max);
        let part: Vec<u8> = (0..take)
            .map(|i| ((self.produced + i) % 251) as u8)
            .collect();
        self.remaining -= take;
        self.produced += take;
        Ok(part)
    }
}

/// Accumulates a downloaded body, which is the only way the suite can compare
/// what came back to what went up.
#[derive(Default)]
struct Collector {
    bytes: Vec<u8>,
}

#[async_trait::async_trait]
impl ChunkSink for Collector {
    async fn write_chunk(&mut self, chunk: &[u8]) -> crate::provider::Result<()> {
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }
}

/// The bytes [`CountingSource`] produces for `len`, regenerated so the
/// download assertion never leans on anything the store returned.
async fn counted_bytes(len: usize) -> Vec<u8> {
    let mut source = CountingSource::new(len);
    let mut out = Vec::with_capacity(len);
    loop {
        let part = source.next_part(4096).await.expect("counting source");
        if part.is_empty() {
            return out;
        }
        out.extend_from_slice(&part);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MockStore;
    use crate::types::ProviderKind;

    #[tokio::test]
    async fn mock_store_passes_contract() {
        for kind in [
            ProviderKind::Aws,
            ProviderKind::DigitalOcean,
            ProviderKind::Gcp,
        ] {
            let store = MockStore::new(kind).with_part_size(32);
            assert_object_store_contract(&store, "contract-bucket").await;
            assert!(
                store.pending_uploads().is_empty(),
                "the contract suite left a multipart upload open on {kind}"
            );
        }
    }

    #[tokio::test]
    async fn mock_store_without_lifecycle_support_still_passes_contract() {
        // The documented-note fallback is a legitimate outcome, not a
        // contract violation.
        let store = MockStore::new(ProviderKind::Local)
            .with_part_size(32)
            .without_lifecycle_support();
        assert_object_store_contract(&store, "contract-bucket").await;
    }

    #[tokio::test]
    async fn contract_exercises_a_real_multipart_upload() {
        let store = MockStore::new(ProviderKind::Aws).with_part_size(32);
        assert_object_store_contract(&store, "b").await;
        let begins = store
            .calls()
            .into_iter()
            .filter(|c| matches!(c, crate::storage::mock::StoreCall::Begin { .. }))
            .count();
        assert_eq!(
            begins, 1,
            "the suite must open exactly one multipart upload"
        );
    }
}
