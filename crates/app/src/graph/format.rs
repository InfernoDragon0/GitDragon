//! Text for graph rows: relative dates, avatar initials and colours.

/// `now`, `5 min ago`, `3 h ago`, `yesterday`, `4 d ago`, then the calendar date
/// (`2024-03-14`, in the commit's own time zone).
pub fn relative_time(time: i64, tz_offset_minutes: i32, now: i64) -> String {
    let ago = now.saturating_sub(time);
    match ago {
        i64::MIN..60 => "now".into(),
        60..3_600 => format!("{} min ago", ago / 60),
        3_600..86_400 => format!("{} h ago", ago / 3_600),
        86_400..172_800 => "yesterday".into(),
        172_800..604_800 => format!("{} d ago", ago / 86_400),
        _ => {
            let local = time.saturating_add(i64::from(tz_offset_minutes) * 60);
            let (y, m, d) = civil_from_days(local.div_euclid(86_400));
            format!("{y:04}-{m:02}-{d:02}")
        }
    }
}

/// Full date and time for tooltips: `2024-03-14 09:26 +0100`.
pub fn full_time(time: i64, tz_offset_minutes: i32) -> String {
    let local = time.saturating_add(i64::from(tz_offset_minutes) * 60);
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let secs = local.rem_euclid(86_400);
    let sign = if tz_offset_minutes < 0 { '-' } else { '+' };
    let tz = tz_offset_minutes.unsigned_abs();
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} {sign}{:02}{:02}",
        secs / 3_600,
        secs % 3_600 / 60,
        tz / 60,
        tz % 60
    )
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Up to two letters: first letters of the first two words, or the first two of one word.
pub fn initials(name: &str) -> String {
    let mut words = name
        .split(|c: char| c.is_whitespace() || matches!(c, '.' | '_' | '-'))
        .filter(|w| !w.is_empty());
    let first: Vec<char> = words
        .next()
        .map(|w| w.chars().collect())
        .unwrap_or_default();
    let letters: String = match words.next().and_then(|w| w.chars().next()) {
        Some(second) => first.iter().take(1).copied().chain([second]).collect(),
        None => first.iter().take(2).collect(),
    };
    if letters.is_empty() {
        "?".into()
    } else {
        letters.to_uppercase()
    }
}

/// Stable palette slot for an author (FNV-1a of the lowercased email).
pub fn avatar_slot(email: &str) -> usize {
    let hash = email.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b.to_ascii_lowercase())).wrapping_mul(0x0100_0000_01b3)
    });
    (hash % 5) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_760_000_000;

    #[test]
    fn relative_times() {
        assert_eq!(relative_time(NOW + 30, 0, NOW), "now");
        assert_eq!(relative_time(NOW - 59, 0, NOW), "now");
        assert_eq!(relative_time(NOW - 300, 0, NOW), "5 min ago");
        assert_eq!(relative_time(NOW - 2 * 3_600, 0, NOW), "2 h ago");
        assert_eq!(relative_time(NOW - 90_000, 0, NOW), "yesterday");
        assert_eq!(relative_time(NOW - 4 * 86_400, 0, NOW), "4 d ago");
        // 2025-10-09 08:53:20 UTC, three weeks earlier.
        assert_eq!(relative_time(NOW - 21 * 86_400, 0, NOW), "2025-09-18");
    }

    #[test]
    fn dates_use_the_commit_time_zone() {
        // 2024-03-14 23:30 UTC is already the 15th at +0100.
        let t = 1_710_459_000;
        assert_eq!(full_time(t, 0), "2024-03-14 23:30 +0000");
        assert_eq!(full_time(t, 60), "2024-03-15 00:30 +0100");
        assert_eq!(full_time(t, -330), "2024-03-14 18:00 -0530");
        assert_eq!(relative_time(t, 60, NOW), "2024-03-15");
    }

    #[test]
    fn calendar_edges() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn initials_from_names() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("mushr"), "MU");
        assert_eq!(initials("j.doe"), "JD");
        assert_eq!(initials("  "), "?");
        assert_eq!(initials("é"), "É");
        assert_eq!(initials("李小龙"), "李小");
    }

    #[test]
    fn avatar_slot_ignores_email_case() {
        assert_eq!(avatar_slot("A@B.c"), avatar_slot("a@b.C"));
        assert!(avatar_slot("x@y.z") < 5);
    }
}
