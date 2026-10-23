//! Recognizes why an agent stopped: usage/rate limits (which some harnesses resume from
//! on their own) and network or transient server failures.

use chrono::{Duration as ChronoDuration, Local, TimeZone};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stop {
    /// Usage or rate limit. `until` is when it is expected to lift (epoch ms), if stated.
    Limit { until: Option<i64> },
    /// Lost connection, timeouts, overloaded or failing API.
    Network,
}

const LIMIT: &[&str] = &[
    "usage limit",
    "usage_limit",
    "rate limit",
    "rate_limit",
    "ratelimit",
    "session limit",
    "weekly limit",
    "hour limit",
    "limit reached",
    "limit exceeded",
    "hit your",
    "quota",
    "resource_exhausted",
    "resource exhausted",
    "too many requests",
    "exhausted your",
    "429",
];

const NETWORK: &[&str] = &[
    "connection",
    "network",
    "econnreset",
    "econnrefused",
    "econnaborted",
    "etimedout",
    "enotfound",
    "eai_again",
    "enetunreach",
    "ehostunreach",
    "socket",
    "timed out",
    "timeout",
    "fetch failed",
    "disconnected",
    "reconnect",
    "offline",
    "unreachable",
    "dns",
    "tls",
    "ssl",
    "certificate",
    "overloaded",
    "server_error",
    "server error",
    "internal error",
    "api_error",
    "service unavailable",
    "bad gateway",
    "unable to connect",
    "502",
    "503",
    "504",
    "529",
];

/// Classifies an error message emitted at `at_ms` (epoch ms).
pub fn classify(text: &str, at_ms: i64) -> Option<Stop> {
    let t = text.to_lowercase();
    if LIMIT.iter().any(|p| t.contains(p)) {
        return Some(Stop::Limit {
            until: reset_time(&t, at_ms),
        });
    }
    if NETWORK.iter().any(|p| t.contains(p)) {
        return Some(Stop::Network);
    }
    None
}

/// Extracts when a limit resets from texts like "resets 10:30pm (Europe/Prague)",
/// "try again at 3:05 PM", "try again in 2 hours 5 minutes" or "limit reached|1760536800".
pub fn reset_time(t: &str, at_ms: i64) -> Option<i64> {
    if let Some(epoch) = pipe_epoch(t) {
        return Some(epoch);
    }
    for key in ["try again in ", "resets in ", "reset in ", "available in "] {
        if let Some(i) = t.find(key) {
            if let Some(d) = duration(&t[i + key.len()..]) {
                return Some(at_ms + d);
            }
        }
    }
    for key in [
        "resets at ",
        "reset at ",
        "resets ",
        "try again at ",
        "available at ",
    ] {
        if let Some(i) = t.find(key) {
            let rest: String = t[i + key.len()..].chars().take(48).collect();
            if let Some((h, m)) = time_of_day(&rest) {
                return next_local(at_ms, h, m);
            }
        }
    }
    None
}

fn pipe_epoch(t: &str) -> Option<i64> {
    let i = t.find('|')?;
    let digits: String = t[i + 1..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.len() == 10 {
        digits.parse::<i64>().ok().map(|s| s * 1000)
    } else {
        None
    }
}

/// Parses "2 hours 13 minutes", "37 min", "1h 5m", "3 days" into milliseconds.
fn duration(s: &str) -> Option<i64> {
    let mut total = 0i64;
    let mut found = false;
    let mut rest = s.trim_start();
    loop {
        let num: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if num.is_empty() {
            break;
        }
        let n: i64 = num.parse().ok()?;
        rest = rest[num.len()..].trim_start();
        let unit: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        let secs = match unit.as_str() {
            "d" | "day" | "days" => 86_400,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            _ => break,
        };
        total += n * secs * 1000;
        found = true;
        rest = rest[unit.len()..].trim_start_matches([' ', ',']);
        rest = rest.strip_prefix("and ").unwrap_or(rest);
    }
    found.then_some(total)
}

/// Finds the first time of day such as "10:30pm", "3am", "3:05 p.m." or "15:00".
fn time_of_day(s: &str) -> Option<(u32, u32)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() || (i > 0 && b[i - 1].is_ascii_alphanumeric()) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < b.len() && b[j].is_ascii_digit() && j - i < 2 {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            i = j;
            continue;
        }
        let hour: u32 = s[i..j].parse().ok()?;
        let mut minute = None;
        let mut k = j;
        if b.get(k) == Some(&b':') {
            let m = s.get(k + 1..k + 3)?;
            if m.bytes().all(|c| c.is_ascii_digit()) {
                minute = m.parse::<u32>().ok();
                k += 3;
            }
        }
        let tail = s[k..].trim_start();
        let meridiem = if tail.starts_with("am") || tail.starts_with("a.m") {
            Some(false)
        } else if tail.starts_with("pm") || tail.starts_with("p.m") {
            Some(true)
        } else {
            None
        };
        match (minute, meridiem) {
            (_, Some(pm)) if (1..=12).contains(&hour) => {
                let h = hour % 12 + if pm { 12 } else { 0 };
                return Some((h, minute.unwrap_or(0)));
            }
            (Some(m), None) if hour < 24 && m < 60 => return Some((hour, m)),
            _ => {}
        }
        i = j;
    }
    None
}

/// Next local occurrence of `h:m` at or after `at_ms`.
fn next_local(at_ms: i64, h: u32, m: u32) -> Option<i64> {
    let at = Local.timestamp_millis_opt(at_ms).single()?;
    let mut day = at.date_naive();
    for _ in 0..2 {
        let naive = day.and_hms_opt(h, m, 0)?;
        if let Some(candidate) = Local.from_local_datetime(&naive).earliest() {
            if candidate.timestamp_millis() >= at_ms - 60_000 {
                return Some(candidate.timestamp_millis());
            }
        }
        day += ChronoDuration::days(1);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        Local
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .earliest()
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn claude_session_limit() {
        let at = local_ms(2026, 10, 21, 18, 21);
        let got = classify(
            "You've hit your session limit · resets 10:30pm (Europe/Prague)",
            at,
        );
        assert_eq!(
            got,
            Some(Stop::Limit {
                until: Some(local_ms(2026, 10, 21, 22, 30))
            })
        );
    }

    #[test]
    fn reset_rolls_over_to_next_day() {
        let at = local_ms(2026, 10, 21, 23, 40);
        let got = reset_time("5-hour limit reached ∙ resets 3am", at);
        assert_eq!(got, Some(local_ms(2026, 10, 22, 3, 0)));
    }

    #[test]
    fn weekly_limit_with_date_uses_time_of_day() {
        let at = local_ms(2026, 10, 21, 8, 0);
        let got = reset_time("weekly limit reached ∙ resets oct 30, 9am", at);
        assert_eq!(got, Some(local_ms(2026, 10, 21, 9, 0)));
    }

    #[test]
    fn codex_try_again_in() {
        let at = 1_000_000;
        let got = classify(
            "You've hit your usage limit. Upgrade to Pro or try again in 2 hours 13 minutes.",
            at,
        );
        assert_eq!(
            got,
            Some(Stop::Limit {
                until: Some(at + (2 * 3600 + 13 * 60) * 1000)
            })
        );
    }

    #[test]
    fn codex_try_again_at() {
        let at = local_ms(2026, 10, 21, 14, 0);
        assert_eq!(
            reset_time("try again at 3:05 pm.", at),
            Some(local_ms(2026, 10, 21, 15, 5))
        );
    }

    #[test]
    fn legacy_epoch_format() {
        assert_eq!(
            reset_time("claude ai usage limit reached|1760536800", 0),
            Some(1_760_536_800_000)
        );
    }

    #[test]
    fn network_errors() {
        for msg in [
            "API Error: Connection error.",
            "stream disconnected before completion",
            "request timed out",
            "fetch failed: ECONNRESET",
            "API Error: 529 Overloaded",
        ] {
            assert_eq!(classify(msg, 0), Some(Stop::Network), "{msg}");
        }
    }

    #[test]
    fn unrelated_errors_are_ignored() {
        assert_eq!(classify("Invalid API key · Please run /login", 0), None);
        assert_eq!(classify("prompt is too long", 0), None);
    }

    #[test]
    fn twenty_four_hour_clock() {
        let at = local_ms(2026, 10, 21, 12, 0);
        assert_eq!(
            reset_time("limit reached, resets at 15:30", at),
            Some(local_ms(2026, 10, 21, 15, 30))
        );
    }
}
