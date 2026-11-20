//! Reads live plan usage for the signed-in account (same data as Claude Code's /usage).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::model::Limit;

const URL: &str = "https://api.anthropic.com/api/oauth/usage";

pub struct Usage {
    pub session: Option<Limit>,
    pub weekly: Option<Limit>,
}

pub enum ApiErr {
    Unauthorized,
    /// Seconds from Retry-After, when the server sent one.
    RateLimited(Option<i64>),
    Other(String),
}

pub(crate) fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let mut b = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).user_agent("Clump/1.0");
        if let Ok(tls) = native_tls::TlsConnector::new() {
            b = b.tls_connector(Arc::new(tls));
        }
        b.build()
    })
}

pub fn fetch(token: &str) -> Result<Usage, ApiErr> {
    let resp = agent()
        .get(URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("Content-Type", "application/json")
        .call();
    let body = match resp {
        Ok(r) => r.into_string().map_err(|e| ApiErr::Other(e.to_string()))?,
        Err(ureq::Error::Status(401 | 403, _)) => return Err(ApiErr::Unauthorized),
        Err(ureq::Error::Status(429, r)) => {
            return Err(ApiErr::RateLimited(r.header("retry-after").and_then(|v| v.trim().parse().ok())))
        }
        Err(ureq::Error::Status(code, _)) => return Err(ApiErr::Other(format!("Usage service answered HTTP {code}"))),
        Err(ureq::Error::Transport(t)) => {
            return Err(ApiErr::Other(match t.kind() {
                ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed => "No connection to Anthropic".into(),
                _ => format!("Network error: {}", t.kind()),
            }))
        }
    };
    parse(&body).ok_or_else(|| ApiErr::Other("Unexpected usage response".into()))
}

fn limit(v: &serde_json::Value) -> Option<Limit> {
    if !v.is_object() {
        return None;
    }
    let pct = v.get("utilization").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let resets_at = v
        .get("resets_at")
        .and_then(|x| x.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp());
    Some(Limit { pct, resets_at })
}

pub fn parse(body: &str) -> Option<Usage> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if !v.is_object() {
        return None;
    }
    Some(Usage { session: v.get("five_hour").and_then(limit), weekly: v.get("seven_day").and_then(limit) })
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_real_response_shape() {
        let body = r#"{"five_hour":{"utilization":5.0,"resets_at":"2026-11-20T06:29:59.514988+00:00","limit_dollars":null},
            "seven_day":{"utilization":66.0,"resets_at":"2026-11-24T19:59:59.515006+00:00"},"seven_day_opus":null}"#;
        let u = super::parse(body).unwrap();
        let s = u.session.unwrap();
        assert_eq!(s.pct, 5.0);
        assert_eq!(s.resets_at, Some(1_795_156_199));
        assert_eq!(u.weekly.unwrap().pct, 66.0);
    }

    #[test]
    fn no_active_window_has_null_reset() {
        let u = super::parse(r#"{"five_hour":{"utilization":0.0,"resets_at":null},"seven_day":null}"#).unwrap();
        assert_eq!(u.session.unwrap().resets_at, None);
        assert!(u.weekly.is_none());
    }
}
