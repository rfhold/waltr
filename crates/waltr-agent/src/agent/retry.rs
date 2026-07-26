//! Provider retry policy for the agent loop.

use std::time::{Duration, SystemTime};

use crate::LlmError;

/// Provider retry configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRetryConfig {
    /// Whether provider retry is enabled.
    pub enabled: bool,
    /// Total provider attempts, including the first request.
    pub max_attempts: u32,
    /// Initial fallback delay before exponential backoff.
    pub initial_delay: Duration,
    /// Exponential backoff multiplier.
    pub backoff_factor: f64,
    /// Maximum fallback delay when the provider gives no retry delay.
    pub fallback_delay_cap: Duration,
    /// Maximum delay honored from provider retry headers.
    pub provider_delay_cap: Duration,
}

impl Default for ProviderRetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attempts: 3,
            initial_delay: Duration::from_secs(2),
            backoff_factor: 2.0,
            fallback_delay_cap: Duration::from_secs(30),
            provider_delay_cap: Duration::from_secs(60),
        }
    }
}

/// Decision for one provider failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRetryDecision {
    /// One-based attempt that failed.
    pub attempt: u32,
    /// Total configured attempts.
    pub max_attempts: u32,
    /// Delay before the next attempt.
    pub delay: Duration,
    /// User-facing retry message.
    pub message: String,
}

impl ProviderRetryConfig {
    /// Decide whether a provider failure should be retried.
    #[must_use]
    pub fn classify(&self, error: &LlmError, attempt: u32) -> Option<ProviderRetryDecision> {
        if !self.enabled || attempt >= self.max_attempts || !is_retryable(error) {
            return None;
        }
        Some(ProviderRetryDecision {
            attempt,
            max_attempts: self.max_attempts,
            delay: retry_after(error)
                .map(|delay| delay.min(self.provider_delay_cap))
                .unwrap_or_else(|| self.fallback_delay(attempt)),
            message: retry_message(error),
        })
    }

    fn fallback_delay(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1) as i32;
        let seconds = self.initial_delay.as_secs_f64() * self.backoff_factor.powi(exponent);
        Duration::from_secs_f64(seconds).min(self.fallback_delay_cap)
    }
}

fn is_retryable(error: &LlmError) -> bool {
    match error {
        LlmError::Http(error) => {
            if error.is_timeout() || error.is_connect() || error.is_request() {
                return true;
            }
            error
                .status()
                .is_some_and(|status| retryable_status(status.as_u16()))
        }
        LlmError::WebSocket(_) => true,
        LlmError::Api {
            message,
            status,
            code,
            ..
        } => {
            if has_terminal_text(message) || code.as_deref().is_some_and(has_terminal_text) {
                return false;
            }
            status.is_some_and(retryable_status)
                || code.as_deref().is_some_and(has_transient_text)
                || has_transient_text(message)
        }
        LlmError::MissingConfig { .. }
        | LlmError::Request(_)
        | LlmError::Json(_)
        | LlmError::Io(_)
        | LlmError::Protocol { .. }
        | LlmError::Authorization { .. }
        | LlmError::McpAuthorization { .. } => false,
    }
}

const fn retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

fn retry_after(error: &LlmError) -> Option<Duration> {
    let LlmError::Api { headers, .. } = error else {
        return None;
    };
    header_value(headers, "retry-after-ms")
        .and_then(parse_retry_after_ms)
        .or_else(|| header_value(headers, "retry-after").and_then(parse_retry_after))
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn parse_retry_after_ms(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_millis)
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value)
        .ok()
        .and_then(|date| date.duration_since(SystemTime::now()).ok())
}

fn retry_message(error: &LlmError) -> String {
    match error {
        LlmError::Api { message, .. } => message.clone(),
        error => error.to_string(),
    }
}

fn has_terminal_text(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "context length",
        "context_length",
        "maximum context",
        "too many tokens",
        "insufficient_quota",
        "quota_exceeded",
        "billing",
        "payment",
        "account",
        "free tier",
        "free-tier",
        "invalid api key",
        "unauthorized",
        "forbidden",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn has_transient_text(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "rate_limit",
        "rate limit",
        "too many requests",
        "temporarily unavailable",
        "overloaded",
        "server error",
        "internal error",
        "bad gateway",
        "service unavailable",
        "gateway timeout",
        "timeout",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_rate_limit_with_provider_delay() {
        let error = LlmError::Api {
            provider: "test",
            message: "rate limited".to_owned(),
            status: Some(429),
            headers: vec![("retry-after-ms".to_owned(), "1500".to_owned())],
            code: None,
            body: None,
        };

        let decision = ProviderRetryConfig::default()
            .classify(&error, 1)
            .expect("rate limit retries");

        assert_eq!(decision.delay, Duration::from_millis(1500));
        assert_eq!(decision.max_attempts, 3);
    }

    #[test]
    fn does_not_retry_terminal_quota_error() {
        let error = LlmError::Api {
            provider: "test",
            message: "insufficient_quota".to_owned(),
            status: Some(429),
            headers: Vec::new(),
            code: None,
            body: None,
        };

        assert!(ProviderRetryConfig::default().classify(&error, 1).is_none());
    }

    #[test]
    fn max_attempts_are_total_attempts() {
        let error = LlmError::Api {
            provider: "test",
            message: "overloaded".to_owned(),
            status: Some(503),
            headers: Vec::new(),
            code: None,
            body: None,
        };

        assert!(ProviderRetryConfig::default().classify(&error, 3).is_none());
    }
}
