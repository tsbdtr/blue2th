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
