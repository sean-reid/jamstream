//! The one days-to-date conversion in the workspace.
//!
//! Not a calendar library, and not to become one: SigV4 signing needs a
//! date stamp, the recorder names takes by the day, and the Takes screen
//! prints one. All three format their own string; the only thing they
//! share is turning days since the epoch into a year, a month, and a day,
//! so that is all this module holds. This crate is the home because the
//! signer cannot do without it and every crate that needs it already
//! depends on this one.

/// Days since 1970-01-01 to (year, month, day), Howard Hinnant's civil
/// calendar algorithm.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known dates, pinned: the epoch, both kinds of leap year (2000 is,
    /// 2100 is not), and the day the rest of the workspace's tests are set
    /// on.
    #[test]
    fn known_days_map_to_their_civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
        assert_eq!(civil_from_days(20_662), (2026, 7, 28));
        // 2100 is divisible by 4 and still not a leap year.
        assert_eq!(civil_from_days(47_540), (2100, 2, 28));
        assert_eq!(civil_from_days(47_541), (2100, 3, 1));
        // The day before the epoch, since the takes screen does the
        // arithmetic in signed days.
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
