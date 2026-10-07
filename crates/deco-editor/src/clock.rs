//! The local date and time, and random numbers, for snippet variables.
//!
//! The standard library gives the time only as UTC and cannot find the local
//! time zone, so the local time is read from the operating system:
//! `localtime_r` on Unix and `GetLocalTime` on Windows. On any other platform
//! the time is UTC.
//!
//! Random numbers come from [`std::hash::RandomState`], whose keys the
//! standard library seeds from the operating system. They are not suitable
//! for cryptography. Snippet variables need values that differ between
//! insertions, not secrets, and VS Code also uses a non-cryptographic source
//! for `RANDOM`.

use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A moment as the local clock shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    /// Seconds since 1970-01-01T00:00:00Z.
    pub unix: i64,
    pub year: i64,
    /// 1 to 12.
    pub month: u32,
    /// 1 to 31.
    pub day: u32,
    /// 0 for Sunday to 6 for Saturday.
    pub weekday: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// Minutes east of UTC.
    pub offset_minutes: i32,
}

impl LocalTime {
    /// The UTC time at `unix`.
    pub fn utc(unix: i64) -> Self {
        Self::at_offset(unix, 0)
    }

    /// The time at `unix` on a clock `offset_minutes` east of UTC.
    pub fn at_offset(unix: i64, offset_minutes: i32) -> Self {
        let local = unix + i64::from(offset_minutes) * 60;
        let days = local.div_euclid(86_400);
        let seconds = local.rem_euclid(86_400) as u32;
        let (year, month, day) = civil_from_days(days);
        Self {
            unix,
            year,
            month,
            day,
            // 1970-01-01 was a Thursday.
            weekday: (days + 4).rem_euclid(7) as u32,
            hour: seconds / 3600,
            minute: seconds / 60 % 60,
            second: seconds % 60,
            offset_minutes,
        }
    }
}

/// The current local time.
pub fn now() -> LocalTime {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    LocalTime::at_offset(unix, platform::offset_minutes(unix).unwrap_or(0))
}

/// A random number, different on every call.
pub fn random_u64() -> u64 {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::hash::RandomState::new().build_hasher();
    hasher.write_u64(CALLS.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos()),
    );
    hasher.finish()
}

/// The date `days` after 1970-01-01, in the proleptic Gregorian calendar.
///
/// The conversion counts 400-year eras of 146,097 days from 0000-03-01, so that
/// the leap day is the last day of each counted year.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_from_march + 2) / 5 + 1) as u32;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Days from 1970-01-01 to the given date; the inverse of [`civil_from_days`].
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_from_march = i64::from(if month > 2 { month - 3 } else { month + 9 });
    let day_of_year = (153 * month_from_march + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Seconds since the epoch of a wall-clock reading, as if it were UTC.
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
fn wall_seconds(year: i64, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
    days_from_civil(year, month, day) * 86_400
        + i64::from(hour) * 3600
        + i64::from(minute) * 60
        + i64::from(second)
}

/// The offset is computed as the difference between the local wall clock and
/// UTC, rounded to whole minutes. Every platform reports the wall clock, while
/// a field holding the offset itself is not available everywhere.
#[cfg(unix)]
mod platform {
    pub fn offset_minutes(unix: i64) -> Option<i32> {
        let time = libc::time_t::try_from(unix).ok()?;
        // SAFETY: `tm` is plain data, so all zeros is a valid value, and
        // `localtime_r` only writes to the struct it is given.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers refer to live locals for the whole call.
        if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
            return None;
        }
        let local = super::wall_seconds(
            i64::from(tm.tm_year) + 1900,
            u32::try_from(tm.tm_mon + 1).ok()?,
            u32::try_from(tm.tm_mday).ok()?,
            u32::try_from(tm.tm_hour).ok()?,
            u32::try_from(tm.tm_min).ok()?,
            // A leap second is reported as 60; it does not change the offset.
            u32::try_from(tm.tm_sec.min(59)).ok()?,
        );
        i32::try_from((local - unix + 30).div_euclid(60)).ok()
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};

    pub fn offset_minutes(_unix: i64) -> Option<i32> {
        // SAFETY: SYSTEMTIME is plain data, so all zeros is a valid value, and
        // both functions only write to the struct they are given.
        let (mut local, mut utc): (SYSTEMTIME, SYSTEMTIME) =
            unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
        // SAFETY: the pointers refer to live locals for the whole call.
        unsafe {
            GetSystemTime(&mut utc);
            GetLocalTime(&mut local);
        }
        let seconds = |time: &SYSTEMTIME| {
            super::wall_seconds(
                i64::from(time.wYear),
                u32::from(time.wMonth),
                u32::from(time.wDay),
                u32::from(time.wHour),
                u32::from(time.wMinute),
                u32::from(time.wSecond),
            )
        };
        // The two calls can straddle a second; rounding to minutes absorbs it.
        i32::try_from((seconds(&local) - seconds(&utc) + 30).div_euclid(60)).ok()
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    pub fn offset_minutes(_unix: i64) -> Option<i32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip_across_leap_years_and_eras() {
        for (days, date) in [
            (0, (1970, 1, 1)),
            (11_016, (2000, 2, 29)),
            (-1, (1969, 12, 31)),
            (20_513, (2026, 3, 1)),
            (-719_468, (0, 3, 1)),
        ] {
            assert_eq!(civil_from_days(days), date, "{days}");
            assert_eq!(days_from_civil(date.0, date.1, date.2), days, "{date:?}");
        }
    }

    #[test]
    fn a_time_is_split_into_its_fields_on_the_local_clock() {
        // 2026-10-04T23:30:15Z, a Sunday, seen from UTC+09:00.
        let time = LocalTime::at_offset(1_791_156_615, 9 * 60);
        assert_eq!(
            (time.year, time.month, time.day, time.weekday),
            (2026, 10, 5, 1),
            "the next day, a Monday, in Tokyo"
        );
        assert_eq!((time.hour, time.minute, time.second), (8, 30, 15));
        let utc = LocalTime::utc(1_791_156_615);
        assert_eq!((utc.day, utc.weekday, utc.hour), (4, 0, 23));
    }

    #[test]
    fn the_local_offset_is_a_plausible_number_of_minutes() {
        let time = now();
        assert!(
            (-14 * 60..=14 * 60).contains(&time.offset_minutes),
            "{time:?}"
        );
    }

    #[test]
    fn random_numbers_differ_between_calls() {
        let numbers: std::collections::BTreeSet<u64> = (0..64).map(|_| random_u64()).collect();
        assert_eq!(numbers.len(), 64);
    }
}
