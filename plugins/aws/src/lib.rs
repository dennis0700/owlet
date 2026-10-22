//! owlet WASM plugin: an MCP proxy for SigV4-protected AWS endpoints.
//!
//! The Rust counterpart of `uvx mcp-proxy-for-aws <endpoint> [--metadata K=V]`:
//! every MCP message from owlet is POSTed to the Streamable HTTP endpoint,
//! signed with SigV4, with metadata merged into `params._meta`. `--read-only`
//! hides and blocks tools without `readOnlyHint: true`; `--profile a b ...`
//! lets auth tools pick the signing profile per call (`aws_profile`), each
//! non-default profile on its own backend session. Configuration
//! comes from WASI argv (see [`cli`]); credentials from the WASI environment
//! and the shared profile files under the preopened `~/.aws`, including role,
//! web identity and SSO profiles (see [`sigv4`]). Outbound HTTP (to the
//! endpoint, STS and the SSO portal) goes through owlet's `owlet:plugin/http`
//! import. On shutdown every live backend session is ended with a `DELETE`.

wit_bindgen::generate!({
    world: "mcp-plugin",
});

mod cli;
mod proxy;
mod sigv4;
mod sse;

use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

struct Plugin;

export!(Plugin);

/// Arguments, parsed on first use.
static OPTIONS: OnceLock<Result<cli::Options, String>> = OnceLock::new();
static SESSION: Mutex<proxy::Session> = Mutex::new(proxy::Session {
    id: None,
    version: None,
    init: None,
    expired: false,
    tools: BTreeMap::new(),
    profiles: BTreeMap::new(),
});
/// Temporary credentials of role and SSO profiles, shared by all sessions.
static CREDENTIALS: Mutex<sigv4::Cache> = Mutex::new(BTreeMap::new());

/// Reads a WASI environment variable; empty values count as unset.
fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

impl Guest for Plugin {
    fn handle(request: String) -> String {
        let options = OPTIONS.get_or_init(|| {
            let args: Vec<String> = std::env::args_os()
                .skip(1)
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            cli::parse(&args, &env)
        });
        let mut session = SESSION.lock().unwrap_or_else(PoisonError::into_inner);
        let mut credentials = CREDENTIALS.lock().unwrap_or_else(PoisonError::into_inner);
        proxy::handle(
            options.as_ref(),
            &mut session,
            &request,
            &env,
            &mut credentials,
            &amz_date(SystemTime::now()),
            owlet::plugin::http::send,
        )
    }

    /// DELETEs the live backend sessions, like `terminate_on_close` in
    /// mcp-proxy-for-aws.
    fn on_shutdown() {
        let Some(Ok(options)) = OPTIONS.get() else {
            return;
        };
        let session = SESSION.lock().unwrap_or_else(PoisonError::into_inner);
        let mut credentials = CREDENTIALS.lock().unwrap_or_else(PoisonError::into_inner);
        proxy::shutdown(
            options,
            &session,
            &env,
            &mut credentials,
            &amz_date(SystemTime::now()),
            owlet::plugin::http::send,
        );
    }
}

/// Formats `time` as the SigV4 `YYYYMMDDTHHMMSSZ` timestamp (UTC).
fn amz_date(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d)
}

/// Proleptic Gregorian date to days since 1970-01-01 (inverse of
/// [`civil_from_days`]); `None` before the epoch.
fn days_from_civil(y: u64, m: u64, d: u64) -> Option<u64> {
    let y = if m <= 2 { y.checked_sub(1)? } else { y };
    let (era, yoe) = (y / 400, y % 400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = ((153 * mp + 2) / 5 + d).checked_sub(1)?;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe).checked_sub(719_468)
}

/// Parses a UTC timestamp to unix seconds: RFC 3339 (`2024-01-01T00:00:00Z`,
/// fractional seconds ignored, `Z` / `UTC` / `+00:00`) or the SigV4 basic
/// form (`20240101T000000Z`).
fn unix_secs(timestamp: &str) -> Option<u64> {
    let t = timestamp.trim();
    let t = t
        .strip_suffix('Z')
        .or_else(|| t.strip_suffix("UTC"))
        .or_else(|| t.strip_suffix("+00:00"))?;
    let t = t.split_once('.').map_or(t, |(whole, _)| whole);
    let digits: String = t.chars().filter(|c| !matches!(c, '-' | ':')).collect();
    let (date, time) = digits.split_once('T')?;
    if date.len() != 8 || time.len() != 6 {
        return None;
    }
    let num = |s: &str, range: std::ops::Range<usize>| {
        let field = s.get(range)?;
        field
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| field.parse::<u64>().ok())?
    };
    let (y, m, d) = (num(date, 0..4)?, num(date, 4..6)?, num(date, 6..8)?);
    let (h, min, s) = (num(time, 0..2)?, num(time, 2..4)?, num(time, 4..6)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 || s > 60 {
        return None;
    }
    Some(days_from_civil(y, m, d)? * 86_400 + h * 3600 + min * 60 + s)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn formats_amz_dates() {
        assert_eq!(amz_date(UNIX_EPOCH), "19700101T000000Z");
        let at = |secs| amz_date(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(951_782_400), "20000229T000000Z");
        assert_eq!(at(1_704_067_199), "20231231T235959Z");
        assert_eq!(at(1_709_208_000), "20240229T120000Z");
    }

    #[test]
    fn parses_utc_timestamps() {
        assert_eq!(unix_secs("19700101T000000Z"), Some(0));
        assert_eq!(unix_secs("20240229T120000Z"), Some(1_709_208_000));
        assert_eq!(unix_secs("2023-12-31T23:59:59Z"), Some(1_704_067_199));
        assert_eq!(unix_secs("2000-02-29T00:00:00.123Z"), Some(951_782_400));
        assert_eq!(unix_secs("2000-02-29T00:00:00UTC"), Some(951_782_400));
        assert_eq!(unix_secs("2000-02-29T00:00:00+00:00"), Some(951_782_400));
        for secs in [0, 951_782_400, 1_704_067_199, 4_102_444_800] {
            let date = amz_date(UNIX_EPOCH + Duration::from_secs(secs));
            assert_eq!(unix_secs(&date), Some(secs), "{date}");
        }
        for bad in [
            "",
            "2024-01-01T00:00:00",
            "2024-13-01T00:00:00Z",
            "2024-01-01T24:00:00Z",
            "1969-12-31T23:59:59Z",
            "2024-01-01 00:00:00Z",
            "20x40101T000000Z",
        ] {
            assert_eq!(unix_secs(bad), None, "{bad}");
        }
    }
}
