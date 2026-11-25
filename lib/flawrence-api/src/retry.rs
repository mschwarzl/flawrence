//! Deciding whether a failed provider call is worth retrying.
//!
//! The signal is read in order of how much we can trust it:
//!
//! 1. the HTTP status code,
//! 2. the provider's structured error `type`/`code`,
//! 3. and only when nothing structured survived the transport, a keyword scan
//!    of the message.
//!
//! Earlier this was step 3 alone, run over the whole stringified error. That
//! misread a transient 503 whose body read "Authentication service temporarily
//! unavailable" as a permanent auth failure, because "authentication" sat in
//! the permanent keyword list. A stringly-typed classifier turns every provider
//! quirk into a new substring and lets unrelated substrings collide (the "500"
//! inside a "50000" token count, say). Reading the status code first removes
//! both problems: a 503 is known to be a 503 without reading its prose.

use flawrence_chat::Usage;

/// Whether a failed call should be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// A transient upstream condition; retrying is expected to help.
    Transient,
    /// A caller or configuration fault; retrying cannot help.
    Permanent,
    /// No signal strong enough to decide. The caller picks the default.
    Unknown,
}

impl RetryClass {
    /// Resolve to a yes/no, using `unknown_is_retryable` for [`Unknown`].
    ///
    /// [`Unknown`]: RetryClass::Unknown
    #[must_use]
    pub fn is_retryable(self, unknown_is_retryable: bool) -> bool {
        match self {
            RetryClass::Transient => true,
            RetryClass::Permanent => false,
            RetryClass::Unknown => unknown_is_retryable,
        }
    }
}

/// Classify a failure from whatever structured facts the transport preserved.
///
/// `status` is the HTTP status, `error_type` and `error_code` are the provider's
/// structured fields (OpenAI `type`/`code`, Anthropic `type`), and `message` is
/// the free text. Pass `None`/empty for whatever is unavailable.
#[must_use]
pub fn classify(
    status: Option<u16>,
    error_type: Option<&str>,
    error_code: Option<&str>,
    message: &str,
) -> RetryClass {
    if let Some(status) = status {
        return classify_status(status);
    }
    if let Some(class) = error_type
        .and_then(classify_structured)
        .or_else(|| error_code.and_then(classify_structured))
    {
        return class;
    }
    classify_message(message)
}

/// HTTP status codes are authoritative: a 5xx is the server's own admission
/// that the fault is on its side, and a 4xx is the request's fault and will
/// fail again identically, with a few exceptions worth retrying:
///
/// - 402 Payment Required: providers use it for a transient billing hiccup as
///   well as a hard one. Retrying costs a little on a genuine out-of-credit
///   account, but a properly behaving provider clears the transient case and
///   the hard case fails no worse than before.
/// - 408 Request Timeout, 425 Too Early, 429 Too Many Requests: transient by
///   definition.
fn classify_status(status: u16) -> RetryClass {
    match status {
        402 | 408 | 425 | 429 => RetryClass::Transient,
        500..=599 => RetryClass::Transient,
        400..=499 => RetryClass::Permanent,
        _ => RetryClass::Unknown,
    }
}

/// Structured `type`/`code` values, as emitted by OpenAI- and
/// Anthropic-compatible providers. These are machine tokens, not prose, so an
/// exact-ish contains check does not suffer the collisions the message scan can.
fn classify_structured(token: &str) -> Option<RetryClass> {
    let token = token.to_ascii_lowercase();
    const PERMANENT: &[&str] = &[
        "invalid_request_error",
        "invalid_api_key",
        "authentication_error",
        "permission_error",
        "permission_denied",
        "not_found_error",
        "unprocessable",
        "unsupported",
    ];
    const TRANSIENT: &[&str] = &[
        "server_error",
        "overloaded_error",
        "overloaded",
        "rate_limit_error",
        "rate_limit",
        "service_unavailable",
        "timeout",
    ];
    if PERMANENT.iter().any(|marker| token.contains(marker)) {
        return Some(RetryClass::Permanent);
    }
    if TRANSIENT.iter().any(|marker| token.contains(marker)) {
        return Some(RetryClass::Transient);
    }
    None
}

/// The last resort, used only when neither a status nor a structured type
/// survived. Explicit transient phrases win over the broad permanent words,
/// because a briefly-down upstream often describes itself in words that also
/// contain a permanent keyword ("authentication service temporarily
/// unavailable"). Bare status numbers are deliberately absent here: a status is
/// read structurally now, and matching a number in free text is what let "500"
/// hide inside "50000".
fn classify_message(message: &str) -> RetryClass {
    let message = message.to_ascii_lowercase();
    const TRANSIENT: &[&str] = &[
        "temporarily unavailable",
        "service unavailable",
        "overloaded",
        "rate limit",
        "too many requests",
        "timeout",
        "timed out",
        "connection reset",
        "connection closed",
        "try again",
    ];
    if TRANSIENT.iter().any(|marker| message.contains(marker)) {
        return RetryClass::Transient;
    }
    const PERMANENT: &[&str] = &[
        "invalid_request",
        "unauthorized",
        "forbidden",
        "permission denied",
        "unknown model",
        "unsupported",
        "not found",
        "authentication",
    ];
    if PERMANENT.iter().any(|marker| message.contains(marker)) {
        return RetryClass::Permanent;
    }
    RetryClass::Unknown
}

/// Message-only classification, for callers that hold nothing but a stringified
/// error (an `anyhow::Error` from a layer that did not preserve the structure).
#[must_use]
pub fn retryable_error_message(message: &str, unknown_is_retryable: bool) -> bool {
    classify(None, None, None, message).is_retryable(unknown_is_retryable)
}

/// A provider error that has already been classified at the boundary, so the
/// task loop reads one decision instead of re-deriving it from a string.
pub trait ClassifiedError {
    /// Whether the call is worth retrying at all.
    fn is_retryable(&self) -> bool;
    /// Whether a retry risks double-billing (the failure may have billed).
    fn billing_unknown(&self) -> bool;
    /// Billed usage carried by the failure, when any was reported.
    fn usage(&self) -> Option<&Usage> {
        None
    }
    /// Retryable, unless a budget is set and a retry could double-bill it.
    fn retryable_with_budget(&self, budget_is_set: bool) -> bool {
        self.is_retryable() && !(budget_is_set && self.billing_unknown())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_code_beats_a_misleading_body() {
        // The reported failure: a 503 whose body says "authentication".
        assert_eq!(
            classify(
                Some(503),
                None,
                None,
                "Authentication service temporarily unavailable"
            ),
            RetryClass::Transient
        );
        // A real auth rejection by status.
        assert_eq!(
            classify(Some(401), None, None, "nope"),
            RetryClass::Permanent
        );
        assert_eq!(classify(Some(429), None, None, ""), RetryClass::Transient);
        assert_eq!(classify(Some(400), None, None, ""), RetryClass::Permanent);
        // 402 Payment Required is retried: some providers use it for a
        // transient billing hiccup. 403 stays permanent.
        assert_eq!(classify(Some(402), None, None, ""), RetryClass::Transient);
        assert_eq!(classify(Some(403), None, None, ""), RetryClass::Permanent);
    }

    #[test]
    fn structured_type_decides_when_there_is_no_status() {
        assert_eq!(
            classify(None, Some("invalid_request_error"), None, "x"),
            RetryClass::Permanent
        );
        assert_eq!(
            classify(None, Some("overloaded_error"), None, "x"),
            RetryClass::Transient
        );
        assert_eq!(
            classify(None, None, Some("rate_limit_exceeded"), "x"),
            RetryClass::Transient
        );
    }

    #[test]
    fn message_is_the_last_resort_and_transient_phrases_win() {
        // No status, no type: fall back to the message, where a transient
        // phrase must beat the permanent "authentication" word.
        assert_eq!(
            classify(
                None,
                None,
                None,
                "Authentication service temporarily unavailable"
            ),
            RetryClass::Transient
        );
        // A genuine bad-key message with no transient phrase stays permanent.
        assert_eq!(
            classify(None, None, None, "401 authentication failed"),
            RetryClass::Permanent
        );
        // Nothing recognisable is Unknown, deferring to the caller's default.
        assert_eq!(
            classify(None, None, None, "weird glitch"),
            RetryClass::Unknown
        );
        assert!(retryable_error_message("weird glitch", true));
        assert!(!retryable_error_message("weird glitch", false));
    }

    #[test]
    fn a_token_count_does_not_look_like_a_500() {
        // The old string scan matched "500" inside "50000"; the message scan no
        // longer carries bare numbers, so a context-length error stays put.
        assert_eq!(
            classify(None, None, None, "prompt is 50000 tokens, over the limit"),
            RetryClass::Unknown
        );
    }
}
