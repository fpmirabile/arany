pub fn utc_date(seconds_since_epoch: u64) -> (u64, u64, u64) {
    // Howard Hinnant's civil date algorithm, using the proleptic Gregorian calendar.
    let shifted = seconds_since_epoch / 86_400 + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

const RESET_FRACTION_DIGITS: usize = 9;
const SECONDS_PER_MINUTE: i64 = 60;
const MINUTES_PER_HOUR: i64 = 60;
const HOURS_PER_DAY: i64 = 24;
const SECONDS_PER_DAY: i64 = SECONDS_PER_MINUTE * MINUTES_PER_HOUR * HOURS_PER_DAY;

pub(crate) fn rfc3339_seconds(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let number = |start: usize, end: usize| {
        let part = bytes.get(start..end)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        Some(
            part.iter()
                .fold(0_i64, |n, byte| n * 10 + i64::from(byte - b'0')),
        )
    };
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    if hour >= HOURS_PER_DAY || minute >= MINUTES_PER_HOUR || second >= SECONDS_PER_MINUTE {
        return None;
    }
    let days = days_since_epoch(year, month, day)?;
    let mut tail = &bytes[19..];
    let mut round_up = false;
    if tail.first() == Some(&b'.') {
        tail = &tail[1..];
        let digits = tail.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if !(1..=RESET_FRACTION_DIGITS).contains(&digits) {
            return None;
        }
        round_up = tail[..digits].iter().any(|byte| *byte != b'0');
        tail = &tail[digits..];
    }
    let offset = utc_offset_seconds(tail)?;
    days.checked_mul(SECONDS_PER_DAY)?
        .checked_add((hour * MINUTES_PER_HOUR + minute) * SECONDS_PER_MINUTE + second)?
        .checked_sub(offset)?
        .checked_add(i64::from(round_up))
}

fn utc_offset_seconds(value: &[u8]) -> Option<i64> {
    match value {
        b"Z" | b"z" => Some(0),
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2]
            if [h1, h2, m1, m2].iter().all(|byte| byte.is_ascii_digit()) =>
        {
            let hours = i64::from((h1 - b'0') * 10 + h2 - b'0');
            let minutes = i64::from((m1 - b'0') * 10 + m2 - b'0');
            if hours >= HOURS_PER_DAY || minutes >= MINUTES_PER_HOUR {
                return None;
            }
            let seconds = (hours * MINUTES_PER_HOUR + minutes) * SECONDS_PER_MINUTE;
            Some(if *sign == b'-' { -seconds } else { seconds })
        }
        _ => None,
    }
}

fn days_since_epoch(year: i64, month: i64, day: i64) -> Option<i64> {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    if !(1..=days_in_month).contains(&day) {
        return None;
    }
    // Inverse of the civil date algorithm used by utc_date.
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    Some(
        era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year
            - 719_468,
    )
}

#[cfg(test)]
mod tests;
