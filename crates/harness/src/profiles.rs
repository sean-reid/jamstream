//! Named network profiles from the testing plan, loaded from
//! `data/profiles.json` at compile time.
//!
//! The plan's `drifting-clock` scenario is not a network profile: it is
//! `regional-fiber` composed with a per-endpoint `SkewedClock`. Network
//! impairment and clock drift are orthogonal knobs in this harness.

use std::sync::LazyLock;

use crate::net::Profile;

static RAW: &str = include_str!("../data/profiles.json");

static PROFILES: LazyLock<Vec<Profile>> =
    LazyLock::new(|| serde_json::from_str(RAW).expect("data/profiles.json parses"));

/// All built-in profiles, in file order.
pub fn all() -> &'static [Profile] {
    &PROFILES
}

/// Looks up a built-in profile by name. Panics on an unknown name; this is a
/// test harness, so a typo should fail loudly, not degrade quietly.
pub fn profile(name: &str) -> &'static Profile {
    all()
        .iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("unknown network profile: {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_expected_profiles_present() {
        let names: Vec<&str> = all().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "lan-fiber",
                "regional-fiber",
                "dsl-cross-country",
                "hostile-wifi",
                "mobile-lte"
            ]
        );
    }

    #[test]
    fn hostile_wifi_has_reordering() {
        let p = profile("hostile-wifi");
        assert_eq!(p.one_way_ms, 10.0);
        assert_eq!(p.jitter_ms, 7.5);
        assert_eq!(p.loss, 0.02);
        assert_eq!(p.reorder_prob, 0.05);
        assert_eq!(p.reorder_extra_ms, 20.0);
        assert_eq!(p.dup_prob, 0.0);
    }

    #[test]
    fn regional_fiber_values() {
        let p = profile("regional-fiber");
        assert_eq!(p.one_way_ms, 6.0);
        assert_eq!(p.jitter_ms, 0.5);
        assert_eq!(p.loss, 0.0005);
    }

    #[test]
    #[should_panic(expected = "unknown network profile")]
    fn unknown_name_panics() {
        profile("drifting-clock");
    }
}
