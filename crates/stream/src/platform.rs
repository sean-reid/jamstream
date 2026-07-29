//! Platform requirements as data. `data/platforms.json` is the only place
//! that knows an ingest URL, and the deferred platforms are documented there
//! with the reason they are not in v1, so adding one is an edit to a data
//! file rather than a change to the pipeline.

use std::collections::BTreeMap;

use jamstream_protocol::control::{StreamKey, StreamPlatform};
use serde::Deserialize;

const PLATFORMS_JSON: &str = include_str!("../data/platforms.json");

/// Encode settings every destination shares. One encode serves all of them,
/// so these are the intersection of what the platforms accept.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VideoSpec {
    pub codec: String,
    pub profile: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub keyframe_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AudioSpec {
    pub codec: String,
    pub kbps: u32,
    pub sample_rate: u32,
    pub channels: u16,
}

/// One destination platform's ingest requirements.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PlatformSpec {
    pub id: String,
    pub display_name: String,
    /// `{key}` is substituted at pusher spawn, from memory, never logged.
    pub ingest_url: String,
    pub requires_cbr: bool,
    pub keyframe_secs: u32,
    pub aspect: String,
    pub max_video_kbps: u32,
    pub max_audio_kbps: u32,
    pub key_acquisition: String,
}

impl PlatformSpec {
    /// The full ingest URL for one key. The result is a secret: it goes to
    /// the pusher's staged spawn file and nowhere else.
    pub fn ingest_url(&self, key: &StreamKey) -> String {
        self.ingest_url.replace("{key}", key.expose())
    }
}

/// A platform documented but not shipped, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeferredPlatform {
    pub id: String,
    pub display_name: String,
    pub aspect: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RawCatalog {
    video: VideoSpec,
    audio: AudioSpec,
    platforms: Vec<PlatformSpec>,
    deferred: Vec<DeferredPlatform>,
}

/// The bundled catalog: encode settings plus every shipped and deferred
/// platform.
#[derive(Debug, Clone)]
pub struct PlatformCatalog {
    video: VideoSpec,
    audio: AudioSpec,
    by_id: BTreeMap<String, PlatformSpec>,
    deferred: Vec<DeferredPlatform>,
}

impl PlatformCatalog {
    /// Parses the bundled data file. Infallible by construction: the file is
    /// compiled in and a malformed edit fails the crate's unit tests.
    pub fn bundled() -> Self {
        let raw: RawCatalog =
            serde_json::from_str(PLATFORMS_JSON).expect("data/platforms.json is invalid");
        PlatformCatalog {
            video: raw.video,
            audio: raw.audio,
            by_id: raw
                .platforms
                .into_iter()
                .map(|p| (p.id.clone(), p))
                .collect(),
            deferred: raw.deferred,
        }
    }

    pub fn video(&self) -> &VideoSpec {
        &self.video
    }

    pub fn audio(&self) -> &AudioSpec {
        &self.audio
    }

    pub fn get(&self, platform: StreamPlatform) -> Option<&PlatformSpec> {
        self.by_id.get(platform.as_str())
    }

    pub fn shipped(&self) -> impl Iterator<Item = &PlatformSpec> {
        self.by_id.values()
    }

    pub fn deferred(&self) -> &[DeferredPlatform] {
        &self.deferred
    }
}

impl Default for PlatformCatalog {
    fn default() -> Self {
        Self::bundled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_wire_platform_has_an_entry() {
        let cat = PlatformCatalog::bundled();
        for p in [StreamPlatform::Twitch, StreamPlatform::YouTube] {
            let spec = cat.get(p).unwrap_or_else(|| panic!("{p:?} missing"));
            assert!(spec.ingest_url.contains("{key}"));
            assert!(spec.ingest_url.starts_with("rtmps://"), "{}", spec.id);
            assert_eq!(spec.aspect, "16:9", "v1 is landscape only");
            assert_eq!(spec.keyframe_secs, 2);
            assert!(!spec.key_acquisition.is_empty());
        }
    }

    #[test]
    fn the_shared_encode_fits_inside_every_platform_ceiling() {
        let cat = PlatformCatalog::bundled();
        assert_eq!(cat.audio().codec, "aac-lc", "no platform takes Opus");
        assert_eq!(cat.audio().sample_rate, 48_000);
        assert_eq!(cat.video().keyframe_secs, 2);
        for spec in cat.shipped() {
            assert!(cat.video().kbps <= spec.max_video_kbps, "{}", spec.id);
            assert!(cat.audio().kbps <= spec.max_audio_kbps, "{}", spec.id);
            assert!(
                cat.video().keyframe_secs <= spec.keyframe_secs,
                "{}",
                spec.id
            );
        }
        // Twitch is the reason the encode is CBR at all.
        assert!(
            cat.get(StreamPlatform::Twitch).unwrap().requires_cbr,
            "the CBR encode exists for Twitch; drop this and the ladder changes"
        );
    }

    #[test]
    fn ingest_url_substitutes_the_key_and_nothing_else() {
        let cat = PlatformCatalog::bundled();
        let spec = cat.get(StreamPlatform::YouTube).unwrap();
        let url = spec.ingest_url(&StreamKey::new("abcd-efgh"));
        assert!(url.ends_with("/live2/abcd-efgh"), "{url}");
        assert!(!url.contains('{'));
    }

    #[test]
    fn deferred_platforms_carry_their_reason() {
        let cat = PlatformCatalog::bundled();
        assert!(cat.deferred().len() >= 4);
        for d in cat.deferred() {
            assert!(!d.reason.is_empty(), "{} needs a reason", d.id);
            // A deferred platform must not shadow a shipped one.
            assert!(!cat.by_id.contains_key(&d.id), "{} is both", d.id);
        }
    }
}
