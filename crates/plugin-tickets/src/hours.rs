use chrono::{DateTime, Datelike, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Weekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl Weekday {
    fn from_monday_index(index: u32) -> Result<Self, &'static str> {
        match index {
            0 => Ok(Self::Monday),
            1 => Ok(Self::Tuesday),
            2 => Ok(Self::Wednesday),
            3 => Ok(Self::Thursday),
            4 => Ok(Self::Friday),
            5 => Ok(Self::Saturday),
            6 => Ok(Self::Sunday),
            _ => Err("invalid weekday"),
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Monday => Self::Sunday,
            Self::Tuesday => Self::Monday,
            Self::Wednesday => Self::Tuesday,
            Self::Thursday => Self::Wednesday,
            Self::Friday => Self::Thursday,
            Self::Saturday => Self::Friday,
            Self::Sunday => Self::Saturday,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawWindow")]
pub(crate) struct Window {
    panel: Option<String>,
    weekday: Weekday,
    start_minute: u16,
    end_minute: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWindow {
    #[serde(default)]
    panel: Option<String>,
    weekday: Weekday,
    start_minute: u16,
    end_minute: u16,
}

impl TryFrom<RawWindow> for Window {
    type Error = &'static str;

    fn try_from(raw: RawWindow) -> Result<Self, Self::Error> {
        Self::for_panel(raw.panel, raw.weekday, raw.start_minute, raw.end_minute)
    }
}

impl Window {
    pub(crate) fn panel_id(&self) -> Option<&str> {
        self.panel.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn new(
        weekday: Weekday,
        start_minute: u16,
        end_minute: u16,
    ) -> Result<Self, &'static str> {
        Self::for_panel(None, weekday, start_minute, end_minute)
    }

    pub(crate) fn for_panel(
        panel: Option<String>,
        weekday: Weekday,
        start_minute: u16,
        end_minute: u16,
    ) -> Result<Self, &'static str> {
        if panel.as_deref().is_some_and(|id| {
            id.is_empty()
                || id.len() > 32
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        }) {
            return Err("invalid panel ID");
        }
        if start_minute >= 1440 {
            return Err("start minute must be between 0 and 1439");
        }
        if end_minute > 1440 {
            return Err("end minute must be between 0 and 1440");
        }
        if start_minute == end_minute {
            return Err("start and end must differ");
        }

        Ok(Self {
            panel,
            weekday,
            start_minute,
            end_minute,
        })
    }
}

#[cfg(test)]
pub(crate) fn is_open(
    timezone: &str,
    days: &[Option<(u16, u16)>; 7],
    unix_seconds: i64,
) -> Result<bool, &'static str> {
    let mut windows = Vec::with_capacity(7);
    for (index, interval) in days.iter().enumerate() {
        if let Some((start_minute, end_minute)) = interval {
            let weekday =
                Weekday::from_monday_index(u32::try_from(index).map_err(|_| "invalid weekday")?)?;
            windows.push(Window::new(weekday, *start_minute, *end_minute)?);
        }
    }

    is_open_windows(timezone, &windows, unix_seconds)
}

#[cfg(test)]
pub(crate) fn is_open_windows(
    timezone: &str,
    windows: &[Window],
    unix_seconds: i64,
) -> Result<bool, &'static str> {
    is_open_for_panel(timezone, None, windows, unix_seconds)
}

pub(crate) fn is_open_for_panel(
    timezone: &str,
    panel: Option<&str>,
    windows: &[Window],
    unix_seconds: i64,
) -> Result<bool, &'static str> {
    let timezone = timezone.parse::<Tz>().map_err(|_| "invalid timezone")?;
    let utc =
        DateTime::<Utc>::from_timestamp(unix_seconds, 0).ok_or("unix timestamp out of range")?;
    if !windows
        .iter()
        .any(|window| window.panel.as_deref() == panel)
    {
        return Ok(true);
    }

    let local = utc.with_timezone(&timezone);
    let weekday = Weekday::from_monday_index(local.weekday().num_days_from_monday())?;
    let previous_day = weekday.previous();
    let minute = local.hour() * 60 + local.minute();

    Ok(windows.iter().any(|window| {
        if window.panel.as_deref() != panel {
            return false;
        }
        let start = u32::from(window.start_minute);
        let end = u32::from(window.end_minute);
        if window.weekday == weekday {
            if start < end {
                start <= minute && minute < end
            } else {
                minute >= start
            }
        } else {
            window.weekday == previous_day && start > end && minute < end
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seconds(value: &str) -> i64 {
        DateTime::parse_from_rfc3339(value)
            .expect("valid RFC 3339 test timestamp")
            .timestamp()
    }

    fn schedule(day: usize, start: u16, end: u16) -> [Option<(u16, u16)>; 7] {
        let mut days = [None; 7];
        days[day] = Some((start, end));
        days
    }

    #[test]
    fn supports_multiple_windows_on_one_day() {
        let windows = [
            Window::new(Weekday::Monday, 9 * 60, 12 * 60).unwrap(),
            Window::new(Weekday::Monday, 13 * 60, 17 * 60).unwrap(),
        ];

        assert!(is_open_windows("UTC", &windows, seconds("2024-01-01T11:59:59Z")).unwrap());
        assert!(!is_open_windows("UTC", &windows, seconds("2024-01-01T12:00:00Z")).unwrap());
        assert!(!is_open_windows("UTC", &windows, seconds("2024-01-01T12:59:59Z")).unwrap());
        assert!(is_open_windows("UTC", &windows, seconds("2024-01-01T13:00:00Z")).unwrap());
        assert!(!is_open_windows("UTC", &windows, seconds("2024-01-01T17:00:00Z")).unwrap());
    }

    #[test]
    fn panel_windows_are_scoped_and_unconfigured_panels_are_open() {
        let windows =
            [
                Window::for_panel(Some("billing".to_owned()), Weekday::Monday, 9 * 60, 17 * 60)
                    .unwrap(),
            ];

        assert!(
            is_open_for_panel(
                "UTC",
                Some("billing"),
                &windows,
                seconds("2024-01-01T10:00:00Z")
            )
            .unwrap()
        );
        assert!(
            !is_open_for_panel(
                "UTC",
                Some("billing"),
                &windows,
                seconds("2024-01-01T18:00:00Z")
            )
            .unwrap()
        );
        assert!(is_open_for_panel("UTC", Some("support"), &windows, 0).unwrap());
        assert!(is_open_for_panel("UTC", None, &windows, 0).unwrap());
    }

    #[test]
    fn uses_inclusive_start_and_exclusive_end() {
        let days = schedule(0, 9 * 60, 12 * 60);

        assert!(!is_open("UTC", &days, seconds("2024-01-01T08:59:59Z")).unwrap());
        assert!(is_open("UTC", &days, seconds("2024-01-01T09:00:00Z")).unwrap());
        assert!(is_open("UTC", &days, seconds("2024-01-01T11:59:59Z")).unwrap());
        assert!(!is_open("UTC", &days, seconds("2024-01-01T12:00:00Z")).unwrap());
    }

    #[test]
    fn overnight_hours_continue_into_the_next_day() {
        let days = schedule(0, 22 * 60, 2 * 60);

        assert!(!is_open("UTC", &days, seconds("2024-01-01T21:59:59Z")).unwrap());
        assert!(is_open("UTC", &days, seconds("2024-01-01T22:00:00Z")).unwrap());
        assert!(is_open("UTC", &days, seconds("2024-01-02T01:59:59Z")).unwrap());
        assert!(!is_open("UTC", &days, seconds("2024-01-02T02:00:00Z")).unwrap());
    }

    #[test]
    fn overnight_hours_cross_the_week_boundary() {
        let days = schedule(6, 22 * 60, 2 * 60);

        assert!(is_open("UTC", &days, seconds("2024-01-07T23:00:00Z")).unwrap());
        assert!(is_open("UTC", &days, seconds("2024-01-08T01:59:59Z")).unwrap());
        assert!(!is_open("UTC", &days, seconds("2024-01-08T02:00:00Z")).unwrap());
    }

    #[test]
    fn no_hours_means_open_all_week() {
        assert!(is_open("UTC", &[None; 7], 0).unwrap());
    }

    #[test]
    fn a_24_hour_window_covers_only_its_selected_day() {
        let days = schedule(0, 0, 1440);

        assert!(is_open("UTC", &days, seconds("2024-01-01T00:00:00Z")).unwrap());
        assert!(is_open("UTC", &days, seconds("2024-01-01T23:59:59Z")).unwrap());
        assert!(!is_open("UTC", &days, seconds("2024-01-02T00:00:00Z")).unwrap());
    }

    #[test]
    fn rejects_invalid_timezone_timestamp_and_schedule() {
        let no_hours = [None; 7];
        assert!(is_open("not/a-timezone", &no_hours, 0).is_err());
        assert!(is_open("UTC", &no_hours, i64::MAX).is_err());
        assert!(Window::new(Weekday::Monday, 60, 60).is_err());
        assert!(Window::new(Weekday::Monday, 60, 1441).is_err());
        assert!(Window::new(Weekday::Monday, 1440, 1440).is_err());
        assert!(Window::for_panel(Some("bad/id".to_owned()), Weekday::Monday, 0, 60).is_err());
        assert!(
            serde_json::from_str::<Window>(
                r#"{"weekday":"monday","start_minute":60,"end_minute":60}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Window>(
                r#"{"weekday":"notaday","start_minute":60,"end_minute":90}"#
            )
            .is_err()
        );
        assert!(is_open("UTC", &schedule(0, 60, 60), 0).is_err());
        assert!(is_open("UTC", &schedule(0, 60, 1441), 0).is_err());
    }

    #[test]
    fn follows_dst_for_iana_timezones() {
        let days = schedule(6, 2 * 60, 3 * 60);

        assert!(!is_open("Europe/London", &days, seconds("2024-03-31T00:30:00Z")).unwrap());
        assert!(is_open("Europe/London", &days, seconds("2024-03-31T01:30:00Z")).unwrap());
        assert!(!is_open("Europe/London", &days, seconds("2024-03-31T02:00:00Z")).unwrap());
    }
}
