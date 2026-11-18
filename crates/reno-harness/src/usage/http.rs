//! The one GET a usage source makes. The token is a header here and nowhere else: not in a log
//! line, not in an error, and not followed to a redirect target.

use std::time::Duration;

use serde_json::Value;

/// How long the endpoint has to answer in all.
const REQUEST_BUDGET: Duration = Duration::from_secs(15);

/// A reply larger than this is not a usage reply.
const MAX_REPLY: usize = 256 * 1024;

/// A client for a usage endpoint: bounded, and it follows no redirect (a `Location` must not be
/// sent a bearer token).
pub(super) fn client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_BUDGET)
        .build()
        .ok()
}

/// GETs `url` with `bearer` and `headers`, and returns the JSON body of a 2xx reply. Everything
/// else is `None` after one log line carrying the provider, the status and the class: a 401 or
/// 403 is `rejected`, never a reason to refresh.
pub(super) async fn get_json(
    client: &reqwest::Client,
    provider: &'static str,
    url: &str,
    bearer: &str,
    headers: &[(&str, &str)],
) -> Option<Value> {
    let mut request = client.get(url).bearer_auth(bearer);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!(
                provider,
                timeout = error.is_timeout(),
                connect = error.is_connect(),
                "plan usage unavailable: network"
            );
            return None;
        }
    };
    let status = response.status();
    if !status.is_success() {
        let class = match status.as_u16() {
            401 | 403 => "rejected",
            429 => "rate limited",
            s if s >= 500 => "server",
            _ => "unexpected status",
        };
        tracing::debug!(
            provider,
            status = status.as_u16(),
            class,
            "plan usage unavailable"
        );
        return None;
    }
    let body = response.bytes().await.ok()?;
    if body.len() > MAX_REPLY {
        tracing::debug!(provider, "plan usage unavailable: reply too large");
        return None;
    }
    let parsed = serde_json::from_slice(&body).ok();
    if parsed.is_none() {
        tracing::debug!(provider, "plan usage unavailable: not JSON");
    }
    parsed
}
