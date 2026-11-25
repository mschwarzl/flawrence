use anyhow::{Context, Result};
use chrono::{NaiveTime, Utc};
use std::sync::Arc;
use tracing::info;

/// A UTC time window during which API calls are allowed.
/// When the current time falls outside this window, callers should
/// sleep until the window opens again.
#[derive(Debug, Clone)]
pub struct Schedule {
    start: NaiveTime,
    end: NaiveTime,
}

/// Shared optional schedule, wrapped in Arc for cheap cloning.
pub type SharedSchedule = Option<Arc<Schedule>>;

impl Schedule {
    /// Parse a schedule string in "HH:MM-HH:MM" format (24-hour UTC).
    /// Supports midnight rollover (e.g. "22:00-06:00").
    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 2 {
            anyhow::bail!(
                "Invalid schedule format '{}'. Expected HH:MM-HH:MM (e.g. 03:00-13:00)",
                s
            );
        }
        let start = NaiveTime::parse_from_str(parts[0].trim(), "%H:%M")
            .with_context(|| format!("Invalid start time '{}'. Expected HH:MM", parts[0].trim()))?;
        let end = NaiveTime::parse_from_str(parts[1].trim(), "%H:%M")
            .with_context(|| format!("Invalid end time '{}'. Expected HH:MM", parts[1].trim()))?;
        Ok(Self { start, end })
    }

    /// Check if the current UTC time is within the allowed window.
    pub fn is_within_window(&self) -> bool {
        let now = Utc::now().time();
        if self.start <= self.end {
            // Same-day window: e.g. 03:00-13:00
            now >= self.start && now < self.end
        } else {
            // Midnight rollover: e.g. 22:00-06:00
            now >= self.start || now < self.end
        }
    }

    /// If outside the window, sleep until the window opens. Returns immediately
    /// if already within the window or if no schedule is set.
    pub async fn wait_for_window(&self) {
        if self.is_within_window() {
            return;
        }

        let now = Utc::now().time();
        let wait_duration = self.duration_until_start(now);
        let secs = wait_duration.num_seconds().max(0);

        info!(
            start = %self.start,
            end = %self.end,
            now = %now.format("%H:%M:%S"),
            wait_minutes = secs / 60,
            "Outside schedule window, pausing API calls"
        );

        tokio::time::sleep(std::time::Duration::from_secs(secs as u64)).await;

        info!("Schedule window opened, resuming API calls");
    }

    /// Calculate how long until self.start from the given time.
    fn duration_until_start(&self, now: NaiveTime) -> chrono::TimeDelta {
        if now < self.start {
            self.start - now
        } else {
            // Wraps past midnight: time remaining today + time into tomorrow
            let until_midnight =
                NaiveTime::from_hms_opt(23, 59, 59).unwrap() - now + chrono::TimeDelta::seconds(1);
            let from_midnight = self.start - NaiveTime::from_hms_opt(0, 0, 0).unwrap();
            until_midnight + from_midnight
        }
    }

    /// Format the schedule for display.
    pub fn display(&self) -> String {
        format!(
            "{}-{} UTC",
            self.start.format("%H:%M"),
            self.end.format("%H:%M")
        )
    }
}

/// If a schedule is set, wait for the window before proceeding.
pub async fn enforce_schedule(schedule: &SharedSchedule) {
    if let Some(sched) = schedule {
        sched.wait_for_window().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_schedule() {
        let s = Schedule::parse("03:00-13:00").unwrap();
        assert_eq!(s.start, NaiveTime::from_hms_opt(3, 0, 0).unwrap());
        assert_eq!(s.end, NaiveTime::from_hms_opt(13, 0, 0).unwrap());
    }

    #[test]
    fn parse_midnight_rollover() {
        let s = Schedule::parse("22:00-06:00").unwrap();
        assert_eq!(s.start, NaiveTime::from_hms_opt(22, 0, 0).unwrap());
        assert_eq!(s.end, NaiveTime::from_hms_opt(6, 0, 0).unwrap());
    }

    #[test]
    fn parse_invalid_format() {
        assert!(Schedule::parse("03:00").is_err());
        assert!(Schedule::parse("abc-def").is_err());
        assert!(Schedule::parse("25:00-13:00").is_err());
    }
}
