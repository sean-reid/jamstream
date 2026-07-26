//! `jamstream status`: every known session with elapsed time, accrued
//! cost, and a projection at the requested horizon.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use jamstream_cloud::format_microusd;

use crate::CliError;
use crate::cli::StatusArgs;
use crate::state::{self, SessionState, SessionStatus};

pub fn run<W: Write>(args: &StatusArgs, out: &mut W) -> Result<(), CliError> {
    let sessions = state::list()?;
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    if args.json {
        let rows: Vec<serde_json::Value> = sessions
            .iter()
            .map(|(_, s)| {
                let elapsed = elapsed_secs(s, now_unix);
                serde_json::json!({
                    "session_id": s.session_id_hex,
                    "provider": s.provider,
                    "region": s.region,
                    "status": s.status,
                    "address": s.address,
                    "created_unix": s.created_unix,
                    "elapsed_secs": elapsed,
                    "hourly_microusd": s.hourly_microusd,
                    "accrued_microusd": cost_for(s.hourly_microusd, elapsed),
                    "projected_microusd": projected(s.hourly_microusd, args.hours),
                })
            })
            .collect();
        writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
        return Ok(());
    }

    if sessions.is_empty() {
        writeln!(out, "No sessions found.")?;
        return Ok(());
    }
    writeln!(
        out,
        "{:<10} {:<20} {:<8} {:>10} {:>12} {:>14}",
        "SESSION", "PROVIDER/REGION", "STATUS", "ELAPSED", "ACCRUED", "PROJECTED"
    )?;
    for (_, s) in &sessions {
        let elapsed = elapsed_secs(s, now_unix);
        let status = match s.status {
            SessionStatus::Running => "running",
            SessionStatus::Ended => "ended",
        };
        let projected = match s.status {
            SessionStatus::Running => {
                format!(
                    "{} at {:.1} h",
                    format_microusd(projected(s.hourly_microusd, args.hours)),
                    args.hours
                )
            }
            SessionStatus::Ended => "-".to_owned(),
        };
        writeln!(
            out,
            "{:<10} {:<20} {:<8} {:>10} {:>12} {:>14}",
            &s.session_id_hex[..8.min(s.session_id_hex.len())],
            format!("{}/{}", s.provider, s.region),
            status,
            format_elapsed(elapsed),
            format_microusd(cost_for(s.hourly_microusd, elapsed)),
            projected,
        )?;
    }
    Ok(())
}

/// Ended sessions stop accruing at ended_unix.
fn elapsed_secs(s: &SessionState, now_unix: u64) -> u64 {
    let until = match s.status {
        SessionStatus::Running => now_unix,
        SessionStatus::Ended => s.ended_unix.unwrap_or(now_unix),
    };
    until.saturating_sub(s.created_unix)
}

fn cost_for(hourly_microusd: u64, secs: u64) -> u64 {
    ((u128::from(hourly_microusd) * u128::from(secs)) / 3600) as u64
}

fn projected(hourly_microusd: u64, hours: f32) -> u64 {
    cost_for(hourly_microusd, (hours.max(0.0) * 3600.0).round() as u64)
}

fn format_elapsed(secs: u64) -> String {
    if secs >= 3600 {
        format!("{} h {:02} min", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{} min", secs / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_math() {
        assert_eq!(cost_for(16_800, 3600), 16_800);
        assert_eq!(cost_for(16_800, 1800), 8_400);
        assert_eq!(projected(16_800, 3.0), 50_400);
        assert_eq!(projected(16_800, 0.0), 0);
    }

    #[test]
    fn elapsed_formatting() {
        assert_eq!(format_elapsed(59), "0 min");
        assert_eq!(format_elapsed(120), "2 min");
        assert_eq!(format_elapsed(3_660), "1 h 01 min");
        assert_eq!(format_elapsed(45_000), "12 h 30 min");
    }
}
