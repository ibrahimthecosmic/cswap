//! The two Anthropic endpoints this tool talks to, and nothing else.
//!
//! Both are undocumented OAuth endpoints that Claude Code itself uses; the
//! constants below were verified against claude-swap 0.25 and Claude Code
//! 2.1.x. Expect them to drift, and keep them here so there is one place to fix.
//!
//! Compiled only with the `usage` feature. Without it this module is absent and
//! the binary has no dependencies and makes no network calls at all.

use std::time::Duration;

use crate::json::Json;
use crate::model::{Usage, Window};
use crate::timefmt;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const USER_AGENT: &str = concat!("cswap/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum ApiError {
    /// The access token was rejected. For a usage call this means "refresh and
    /// retry"; it says nothing about the refresh token.
    Unauthorized,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "token rejected"),
            ApiError::RateLimited => write!(f, "rate limited"),
            ApiError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .user_agent(USER_AGENT)
        .build()
        .into()
}

fn classify(status: u16, body: &str) -> ApiError {
    match status {
        401 | 403 => ApiError::Unauthorized,
        429 => ApiError::RateLimited,
        _ => ApiError::Other(format!("HTTP {status}: {}", truncate(body, 120))),
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

pub fn fetch_usage(access_token: &str) -> Result<Usage, ApiError> {
    let mut response = agent()
        .get(USAGE_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-beta", OAUTH_BETA)
        .call()
        .map_err(|e| ApiError::Other(e.to_string()))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| ApiError::Other(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(classify(status, &body));
    }
    let data = Json::parse(&body).map_err(|e| ApiError::Other(format!("bad response: {e}")))?;
    Ok(parse_usage(&data))
}

fn parse_usage(data: &Json) -> Usage {
    let window = |key: &str, label: &str| -> Option<Window> {
        let w = data.get(key)?;
        Some(Window {
            label: label.to_string(),
            pct: w.get_f64("utilization")?,
            resets_at: w.get_str("resets_at").and_then(timefmt::parse_rfc3339),
        })
    };

    // Per-model weekly limits live in the newer `limits` array, carrying a
    // scope.model.display_name such as "Fable". The legacy five_hour/seven_day
    // keys never expose these, and older responses omit `limits` entirely.
    let mut scoped = Vec::new();
    if let Some(limits) = data.get("limits").and_then(Json::as_arr) {
        for limit in limits {
            let name = limit
                .get("scope")
                .and_then(|s| s.get("model"))
                .and_then(|m| m.get_str("display_name"));
            if let (Some(name), Some(pct)) = (name, limit.get_f64("percent")) {
                scoped.push(Window {
                    label: name.to_string(),
                    pct,
                    resets_at: limit.get_str("resets_at").and_then(timefmt::parse_rfc3339),
                });
            }
        }
    }

    Usage {
        five_hour: window("five_hour", "5h"),
        seven_day: window("seven_day", "7d"),
        scoped,
    }
}

pub enum Refresh {
    /// The full credential blob with a rotated access token. **Must be persisted
    /// before it is used for anything else** — see `usage::token_for`.
    Rotated(String),
    /// The refresh lineage is dead: the server rejected the grant outright.
    /// Only re-logging in with Claude Code and re-running `cswap add` fixes it.
    Dead,
    /// Anything ambiguous. A misclassified transient costs one retry; a
    /// misclassified permanent would condemn a live account.
    Transient(String),
}

/// Exchange a credential's refresh token for a fresh access token.
///
/// Refresh tokens are single-use: a successful call invalidates the one that was
/// sent. Whatever comes back has to reach disk before anything else can fail, or
/// the account is stranded.
pub fn refresh(credentials: &str) -> Refresh {
    let Ok(mut data) = Json::parse(credentials) else {
        return Refresh::Transient("stored credential is not JSON".into());
    };
    let Some(oauth) = data.get("claudeAiOauth").cloned() else {
        return Refresh::Transient("stored credential has no claudeAiOauth block".into());
    };
    let Some(refresh_token) = oauth.get_str("refreshToken") else {
        return Refresh::Dead;
    };

    let mut body = Json::obj();
    body.set("grant_type", Json::str("refresh_token"));
    body.set("refresh_token", Json::str(refresh_token));
    body.set("client_id", Json::str(CLIENT_ID));

    let mut response = match agent()
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .send(body.dump())
    {
        Ok(r) => r,
        Err(e) => return Refresh::Transient(e.to_string()),
    };
    let status = response.status().as_u16();
    let text = match response.body_mut().read_to_string() {
        Ok(t) => t,
        Err(e) => return Refresh::Transient(e.to_string()),
    };

    if !(200..300).contains(&status) {
        // RFC 6749 §5.2: the verdict is the top-level `error` member. Only a
        // 4xx *with* an explicit invalid_grant is proof the lineage is dead;
        // invalid_client means our client_id was rejected, which is systemic
        // and says nothing about this account.
        if matches!(status, 400 | 401 | 403) {
            if let Ok(err) = Json::parse(&text) {
                if err.get_str("error") == Some("invalid_grant") {
                    return Refresh::Dead;
                }
            }
        }
        return Refresh::Transient(format!("HTTP {status}: {}", truncate(&text, 120)));
    }

    let Ok(payload) = Json::parse(&text) else {
        return Refresh::Transient("token endpoint returned non-JSON".into());
    };
    let Some(access_token) = payload.get_str("access_token") else {
        return Refresh::Transient("token endpoint returned no access_token".into());
    };

    let mut oauth = oauth;
    oauth.set("accessToken", Json::str(access_token));
    if let Some(expires_in) = payload.get_i64("expires_in") {
        oauth.set(
            "expiresAt",
            Json::num(timefmt::now_unix_ms() + expires_in * 1000),
        );
    }
    if let Some(rotated) = payload.get_str("refresh_token") {
        oauth.set("refreshToken", Json::str(rotated));
    }
    if let Some(scope) = payload.get_str("scope") {
        oauth.set(
            "scopes",
            Json::Arr(scope.split_whitespace().map(Json::str).collect()),
        );
    }
    data.set("claudeAiOauth", oauth);
    Refresh::Rotated(data.dump())
}
