// SPDX-License-Identifier: MIT OR Apache-2.0

use super::backend_error_text;
use crate::backend::BackendError;

// Criterion (#196): a refused key reaches the user as the localised
// identity-changed message, which names the fix — pairing again — rather than
// as the error's own English text, which names none. Both locales say "pair"
// ("pair it again", "appairez-le"), so the claim holds whichever one is set.
#[test]
fn test_backend_error_text_of_an_identity_change_says_to_pair_again() {
    let text = backend_error_text(&BackendError::identity_changed());
    assert_ne!(text, crate::backend::IDENTITY_CHANGED);
    assert!(
        text.contains("pair"),
        "the message must name the fix: {text}"
    );
}

// Criterion: any other failure keeps its own wording — here a refused token,
// whose text is `backend::NOT_PAIRED`.
#[test]
fn test_backend_error_text_of_another_failure_keeps_its_own_message() {
    assert_eq!(
        backend_error_text(&BackendError::not_paired()),
        "not paired"
    );
}

// Criterion (#197): a refused pairing — a 401 on `start` or `finish`, or a
// backend confirmation that does not check out — reaches the user as the
// localised refusal, on the surface pairing errors use today, rather than as
// the error's own English text. Both locales name the command that mints a
// new code, `blue2th-server --pair`, so the claim holds whichever is set.
#[test]
fn test_backend_error_text_of_a_refused_pairing_names_the_pair_command() {
    let text = backend_error_text(&BackendError::pairing_refused());
    assert_ne!(text, crate::backend::PAIRING_REFUSED);
    assert!(
        text.contains("blue2th-server --pair"),
        "the message must name the remedy: {text}"
    );
}

// Criterion (#197): a rate-limited pairing (429 on `start`) reaches the user
// as its own localised message — neither the error's English text nor the
// refusal's wording, since waiting, not a new code, is the remedy.
#[test]
fn test_backend_error_text_of_a_rate_limited_pairing_is_its_own_message() {
    let text = backend_error_text(&BackendError::pairing_rate_limited());
    assert_ne!(text, crate::backend::PAIRING_RATE_LIMITED);
    assert_ne!(
        text,
        backend_error_text(&BackendError::pairing_refused()),
        "a rate limit is not a refusal"
    );
    assert!(!text.is_empty());
}
