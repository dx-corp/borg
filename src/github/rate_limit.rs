use std::sync::{Arc, Mutex};

use reqwest::{RequestBuilder, Response, header::HeaderMap};
use serde::Serialize;

use super::{GithubError, unix_now};

#[derive(Clone, Debug, Default, Serialize)]
pub struct RateLimitView {
    limit: Option<u64>,
    pub(crate) remaining: Option<u64>,
    pub(crate) reset_at: Option<i64>,
    pub(crate) retry_at: Option<i64>,
}

#[derive(Default)]
struct State {
    view: RateLimitView,
    secondary_failures: u32,
}

#[derive(Clone, Default)]
pub struct RateLimit(Arc<Mutex<State>>);

impl RateLimit {
    pub(crate) fn view(&self) -> RateLimitView {
        self.0.lock().expect("GitHub quota lock").view.clone()
    }

    fn check_at(&self, now: i64) -> Result<(), GithubError> {
        if let Some(retry_at) = self.view().retry_at.filter(|at| *at > now) {
            return Err(GithubError::RateLimited { retry_at });
        }
        Ok(())
    }

    fn defer(state: &mut State, until: i64) -> i64 {
        let until = state.view.retry_at.unwrap_or_default().max(until);
        state.view.retry_at = Some(until);
        until
    }

    fn secondary_at(&self, now: i64) -> GithubError {
        let mut state = self.0.lock().expect("GitHub quota lock");
        if let Some(retry_at) = state.view.retry_at.filter(|at| *at > now) {
            return GithubError::RateLimited { retry_at };
        }
        let delay = 60_i64.saturating_mul(1_i64 << state.secondary_failures.min(6));
        state.secondary_failures = state.secondary_failures.saturating_add(1);
        let retry_at = Self::defer(&mut state, now.saturating_add(delay).saturating_add(1));
        GithubError::RateLimited { retry_at }
    }

    fn observe_at(&self, status: u16, headers: &HeaderMap, now: i64) -> Result<(), GithubError> {
        let number = |key| {
            headers
                .get(key)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
        };
        let remaining = number("x-ratelimit-remaining");
        let reset = number("x-ratelimit-reset").and_then(|v| i64::try_from(v).ok());
        let retry = number("retry-after").and_then(|v| i64::try_from(v).ok());
        let exhausted = remaining == Some(0);
        let limited =
            matches!(status, 403 | 429) && (exhausted || retry.is_some() || status == 429);
        let mut state = self.0.lock().expect("GitHub quota lock");
        if let Some(limit) = number("x-ratelimit-limit") {
            state.view.limit = Some(limit);
        }
        if remaining.is_some() {
            state.view.remaining = remaining;
        }
        if reset.is_some() {
            state.view.reset_at = reset;
        }
        let reset_delay = exhausted.then(|| {
            reset
                .filter(|at| *at > now)
                .unwrap_or_else(|| now.saturating_add(60))
                .saturating_add(1)
        });
        // Epoch seconds truncate the response time; keep the full relative wait.
        let retry_delay = retry.map(|seconds| now.saturating_add(seconds.max(1)).saturating_add(1));
        if let Some(until) = reset_delay.into_iter().chain(retry_delay).max() {
            let retry_at = Self::defer(&mut state, until);
            if limited {
                return Err(GithubError::RateLimited { retry_at });
            }
        } else if limited {
            drop(state);
            return Err(self.secondary_at(now));
        } else if (200..300).contains(&status) && state.view.retry_at.is_none_or(|at| at <= now) {
            state.secondary_failures = 0;
            state.view.retry_at = None;
        }
        Ok(())
    }

    pub(crate) async fn send(&self, request: RequestBuilder) -> Result<Response, GithubError> {
        self.check_at(unix_now())?;
        let response = request.send().await?;
        self.observe_at(response.status().as_u16(), response.headers(), unix_now())?;
        Ok(response)
    }

    pub(crate) fn error(
        &self,
        method: &'static str,
        path: &str,
        status: u16,
        body: &str,
    ) -> GithubError {
        // Secondary limits may have no Retry-After header. Ordinary permission
        // failures retain their existing classification.
        if status == 403
            && serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| {
                    v.get("message")
                        .and_then(|v| v.as_str())
                        .map(str::to_ascii_lowercase)
                })
                .is_some_and(|message| {
                    message.contains("rate limit") || message.contains("abuse detection")
                })
        {
            return self.secondary_at(unix_now());
        }
        GithubError::Status {
            method,
            path: path.to_owned(),
            status,
            body: body.chars().take(200).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[(&str, &str)]) -> HeaderMap {
        values
            .iter()
            .map(|(k, v)| {
                (
                    reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    v.parse().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn exhausted_quota_blocks_shared_requests_until_reset_without_shortening_cooldown() {
        let limit = RateLimit::default();
        let shared = limit.clone();
        assert!(matches!(
            limit.observe_at(
                403,
                &headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "400")]),
                100
            ),
            Err(GithubError::RateLimited { retry_at: 401 })
        ));
        // An older concurrent success cannot clear an active cooldown.
        shared
            .observe_at(200, &headers(&[("x-ratelimit-remaining", "7")]), 101)
            .unwrap();
        assert!(shared.check_at(400).is_err());
        assert!(shared.check_at(401).is_ok());
        assert!(
            RateLimit::default().check_at(100).is_ok(),
            "independent credentials are not blocked"
        );
    }

    #[test]
    fn successful_last_budget_mutation_is_returned_and_next_request_is_blocked() {
        let limit = RateLimit::default();
        limit
            .observe_at(
                201,
                &headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "200")]),
                100,
            )
            .unwrap();
        assert!(limit.check_at(100).is_err());
    }

    #[test]
    fn retry_after_and_reset_use_the_later_deadline() {
        let limit = RateLimit::default();
        assert!(matches!(
            limit.observe_at(
                429,
                &headers(&[
                    ("retry-after", "120"),
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", "200")
                ]),
                100
            ),
            Err(GithubError::RateLimited { retry_at: 221 })
        ));
    }

    #[test]
    fn secondary_backoff_grows_between_attempts_not_concurrent_responses() {
        let limit = RateLimit::default();
        assert!(matches!(
            limit.observe_at(429, &HeaderMap::new(), 100),
            Err(GithubError::RateLimited { retry_at: 161 })
        ));
        assert!(matches!(
            limit.observe_at(429, &HeaderMap::new(), 101),
            Err(GithubError::RateLimited { retry_at: 161 })
        ));
        assert!(matches!(
            limit.observe_at(429, &HeaderMap::new(), 161),
            Err(GithubError::RateLimited { retry_at: 282 })
        ));
        limit.observe_at(200, &HeaderMap::new(), 282).unwrap();
        assert!(matches!(
            limit.observe_at(429, &HeaderMap::new(), 300),
            Err(GithubError::RateLimited { retry_at: 361 })
        ));
    }

    #[test]
    fn malformed_reset_still_defers_but_permission_refusal_does_not() {
        let limit = RateLimit::default();
        limit.observe_at(403, &HeaderMap::new(), 100).unwrap();
        assert!(matches!(
            limit.error(
                "GET",
                "/resource",
                403,
                r#"{"message":"Resource not accessible by integration"}"#
            ),
            GithubError::Status { status: 403, .. }
        ));
        assert!(limit.check_at(100).is_ok());
        assert!(
            limit
                .observe_at(
                    403,
                    &headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "bad")]),
                    100
                )
                .is_err()
        );
        assert!(limit.check_at(160).is_err());
        assert!(limit.check_at(161).is_ok());
    }

    #[test]
    fn secondary_message_without_retry_header_is_recognized() {
        let limit = RateLimit::default();
        assert!(matches!(
            limit.error(
                "POST",
                "/rerun",
                403,
                r#"{"message":"You have exceeded a secondary rate limit."}"#
            ),
            GithubError::RateLimited { .. }
        ));
        assert!(limit.check_at(unix_now()).is_err());
    }
}
