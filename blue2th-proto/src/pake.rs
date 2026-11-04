// SPDX-License-Identifier: MIT OR Apache-2.0

//! The pairing exchange of #197: SPAKE2 run on the pairing code, its result
//! bound to the TLS key the app saw.
//!
//! Shared by the native app (the [`ClientHalf`], SPAKE2's side A) and the
//! backend (the [`ServerHalf`], side B), so both compute the same bytes from one
//! source. Behind the non-default `pake` feature: the browser build pairs over
//! `POST /api/pair` and never compiles it.

/// SPAKE2 identity of the app, side A.
pub const APP_IDENTITY: &str = "blue2th-app";

/// SPAKE2 identity of the backend, side B.
pub const SERVER_IDENTITY: &str = "blue2th-server";

/// What the exchange is bound to in the plain-HTTP debug mode, where there is
/// no TLS key to bind it to.
pub const PLAIN_HTTP_LABEL: &str = "blue2th-plain-http";

/// Length of a SPAKE2 message on `Ed25519Group`.
pub const MESSAGE_LEN: usize = 33;

/// Length of the key the exchange derives.
pub const KEY_LEN: usize = 32;

/// Length of a confirmation.
pub const CONFIRM_LEN: usize = 32;

/// What a confirmation is computed over: the SHA-256 of the backend's SPKI DER,
/// or the plain-HTTP label. There is no empty binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// The SHA-256 of the backend's `SubjectPublicKeyInfo` DER.
    Spki([u8; 32]),
    /// The plain-HTTP debug mode: [`PLAIN_HTTP_LABEL`].
    PlainHttp,
}

impl Binding {
    /// The binding of a stored pin — 64 lower-case hex characters, the format
    /// of the app's `Pin::to_hex` — or `None` for anything else.
    pub fn from_pin_hex(_pin: &str) -> Option<Binding> {
        None
    }
}

/// The exchange failed. All the caller learns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PakeError;

impl std::fmt::Display for PakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pairing exchange failed")
    }
}

impl std::error::Error for PakeError {}

/// The key both halves derive when they ran the exchange on the same code.
pub struct SharedKey([u8; KEY_LEN]);

impl SharedKey {
    /// The key's bytes.
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl std::fmt::Debug for SharedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the bytes: a key in a log line is a key given away.
        write!(f, "SharedKey(..)")
    }
}

/// The app's half of the exchange, SPAKE2's side A.
pub struct ClientHalf {
    _state: (),
}

impl ClientHalf {
    /// Start the app's side on `code`, normalized as
    /// [`crate::normalize_pairing_code`] does: the half, and the message to
    /// send.
    pub fn start(_code: &str) -> (ClientHalf, Vec<u8>) {
        (ClientHalf { _state: () }, Vec::new())
    }

    /// Finish with the backend's message into the shared key.
    pub fn finish(self, _server_msg: &[u8]) -> Result<SharedKey, PakeError> {
        Err(PakeError)
    }
}

/// The backend's half of the exchange, SPAKE2's side B.
pub struct ServerHalf {
    _state: (),
}

impl ServerHalf {
    /// Start the backend's side on `code`, normalized as
    /// [`crate::normalize_pairing_code`] does: the half, and the message to
    /// send.
    pub fn start(_code: &str) -> (ServerHalf, Vec<u8>) {
        (ServerHalf { _state: () }, Vec::new())
    }

    /// Finish with the app's message into the shared key.
    pub fn finish(self, _client_msg: &[u8]) -> Result<SharedKey, PakeError> {
        Err(PakeError)
    }
}

/// The app's confirmation: a MAC under `key` over `binding`.
pub fn confirm_a(_key: &SharedKey, _binding: &Binding) -> [u8; CONFIRM_LEN] {
    [0; CONFIRM_LEN]
}

/// The backend's confirmation: a MAC under `key` over `binding`, under another
/// label than [`confirm_a`]'s.
pub fn confirm_b(_key: &SharedKey, _binding: &Binding) -> [u8; CONFIRM_LEN] {
    [0; CONFIRM_LEN]
}

/// Whether `presented` is the app's confirmation under `key` over `binding`,
/// compared in constant time.
pub fn check_confirm_a(_key: &SharedKey, _binding: &Binding, _presented: &[u8]) -> bool {
    false
}

/// Whether `presented` is the backend's confirmation under `key` over
/// `binding`, compared in constant time.
pub fn check_confirm_b(_key: &SharedKey, _binding: &Binding, _presented: &[u8]) -> bool {
    false
}

/// Whether `a` and `b` are equal, without an early exit on the first differing
/// byte.
pub fn constant_time_eq(_a: &[u8], _b: &[u8]) -> bool {
    false
}

/// Lower-case hex of `bytes`, the wire form of every field of the exchange.
pub fn to_hex(_bytes: &[u8]) -> String {
    String::new()
}

/// The bytes of `hex`, when it is hex for exactly `len` bytes; `None`
/// otherwise.
pub fn from_hex(_hex: &str, _len: usize) -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests;
