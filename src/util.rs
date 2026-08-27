//! Small cross-cutting helpers shared by the wrapper and the proxy.

use std::time::{SystemTime, UNIX_EPOCH};

/// Formats a context window the way the old bash script did: 828400 → "828K",
/// 1000000 → "1M", 500 → "500".
pub fn fmt_ctx(c: u64) -> String {
    if c >= 1_000_000 {
        format!("{}M", c / 1_000_000)
    } else if c >= 1_000 {
        format!("{}K", c / 1_000)
    } else {
        format!("{c}")
    }
}

/// Milliseconds since the Unix epoch (used for the synthetic msg_* ids, like
/// the node proxy's Date.now()).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Seconds since the Unix epoch (cache freshness checks).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current UTC time as an RFC 3339 string (for `last_refresh` in auth.json).
/// Written by hand to avoid a chrono/time dependency; days-to-civil is the
/// standard Howard Hinnant algorithm.
pub fn iso_now() -> String {
    let secs = now_unix() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Decodes the payload (second segment) of a JWT as JSON. Used to read the
/// `client_id` bound to a Codex OAuth token.
pub fn jwt_payload(token: &str) -> Option<serde_json::Value> {
    use base64::Engine as _;
    let segment = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(segment)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_context() {
        assert_eq!(fmt_ctx(828_400), "828K");
        assert_eq!(fmt_ctx(1_000_000), "1M");
        assert_eq!(fmt_ctx(2_600_000), "2M");
        assert_eq!(fmt_ctx(500), "500");
        assert_eq!(fmt_ctx(0), "0");
    }

    #[test]
    fn decodes_jwt_payload() {
        // base64url("{\"client_id\":\"abc\"}")
        let token = "eyJhbGciOiJub25lIn0.eyJjbGllbnRfaWQiOiJhYmMifQ.";
        let payload = jwt_payload(token).expect("payload decodes");
        assert_eq!(payload["client_id"], "abc");
        assert!(jwt_payload("not-a-jwt").is_none());
        assert!(jwt_payload("a.@@@.c").is_none());
    }

    #[test]
    fn iso_now_is_rfc3339() {
        let s = iso_now();
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z'));
        assert_eq!(&s[10..11], "T");
    }
}
