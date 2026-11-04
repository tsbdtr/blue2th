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
    pub fn from_pin_hex(pin: &str) -> Option<Binding> {
        if pin.bytes().any(|b| b.is_ascii_uppercase()) {
            return None;
        }
        let bytes = from_hex(pin, 32)?;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes);
        Some(Binding::Spki(hash))
    }

    /// The bytes a confirmation is computed over. Tagged, so that no SPKI hash
    /// can ever encode the same bytes as the label.
    fn encoded(&self) -> Vec<u8> {
        match self {
            Binding::Spki(hash) => {
                let mut out = Vec::with_capacity(1 + hash.len());
                out.push(0x01);
                out.extend_from_slice(hash);
                out
            },
            Binding::PlainHttp => {
                let mut out = Vec::with_capacity(1 + PLAIN_HTTP_LABEL.len());
                out.push(0x02);
                out.extend_from_slice(PLAIN_HTTP_LABEL.as_bytes());
                out
            },
        }
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

    /// The raw SPAKE2 output as a key, refused unless it is [`KEY_LEN`] bytes.
    fn from_raw(raw: &[u8]) -> Result<SharedKey, PakeError> {
        let mut key = [0u8; KEY_LEN];
        if raw.len() != KEY_LEN {
            return Err(PakeError);
        }
        key.copy_from_slice(raw);
        Ok(SharedKey(key))
    }
}

impl std::fmt::Debug for SharedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the bytes: a key in a log line is a key given away.
        write!(f, "SharedKey(..)")
    }
}

type Spake = spake2::Spake2<spake2::Ed25519Group>;

/// The SPAKE2 password and the two identities, from `code`.
fn inputs(code: &str) -> (spake2::Password, spake2::Identity, spake2::Identity) {
    let password = crate::normalize_pairing_code(code);
    (
        spake2::Password::new(password.as_bytes()),
        spake2::Identity::new(APP_IDENTITY.as_bytes()),
        spake2::Identity::new(SERVER_IDENTITY.as_bytes()),
    )
}

/// Finish `state` with the peer's `msg`, refusing a message of the wrong length
/// before SPAKE2 sees it.
fn finish(state: Spake, msg: &[u8]) -> Result<SharedKey, PakeError> {
    if msg.len() != MESSAGE_LEN {
        return Err(PakeError);
    }
    let raw = state.finish(msg).map_err(|_| PakeError)?;
    SharedKey::from_raw(&raw)
}

/// The app's half of the exchange, SPAKE2's side A.
pub struct ClientHalf {
    state: Spake,
}

impl ClientHalf {
    /// Start the app's side on `code`, normalized as
    /// [`crate::normalize_pairing_code`] does: the half, and the message to
    /// send.
    pub fn start(code: &str) -> (ClientHalf, Vec<u8>) {
        let (password, app, server) = inputs(code);
        let (state, msg) = Spake::start_a(&password, &app, &server);
        (ClientHalf { state }, msg)
    }

    /// Finish with the backend's message into the shared key.
    pub fn finish(self, server_msg: &[u8]) -> Result<SharedKey, PakeError> {
        finish(self.state, server_msg)
    }
}

/// The backend's half of the exchange, SPAKE2's side B.
pub struct ServerHalf {
    state: Spake,
}

impl ServerHalf {
    /// Start the backend's side on `code`, normalized as
    /// [`crate::normalize_pairing_code`] does: the half, and the message to
    /// send.
    pub fn start(code: &str) -> (ServerHalf, Vec<u8>) {
        let (password, app, server) = inputs(code);
        let (state, msg) = Spake::start_b(&password, &app, &server);
        (ServerHalf { state }, msg)
    }

    /// Finish with the app's message into the shared key.
    pub fn finish(self, client_msg: &[u8]) -> Result<SharedKey, PakeError> {
        finish(self.state, client_msg)
    }
}

/// Domain-separation label of the app's confirmation.
const CONFIRM_A_LABEL: &[u8] = b"blue2th-pake-confirm-a";

/// Domain-separation label of the backend's confirmation.
const CONFIRM_B_LABEL: &[u8] = b"blue2th-pake-confirm-b";

/// HMAC-SHA256 under `key` over `label` then `binding`.
fn mac(key: &SharedKey, label: &[u8], binding: &Binding) -> [u8; CONFIRM_LEN] {
    use hmac::Mac as _;

    let mut out = [0u8; CONFIRM_LEN];
    // HMAC takes a key of any length, so this never fails; were it to, the
    // all-zero confirmation is refused by every check.
    if let Ok(mut hmac) = hmac::Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes()) {
        hmac.update(label);
        hmac.update(&binding.encoded());
        out.copy_from_slice(&hmac.finalize().into_bytes());
    }
    out
}

/// The app's confirmation: a MAC under `key` over `binding`.
pub fn confirm_a(key: &SharedKey, binding: &Binding) -> [u8; CONFIRM_LEN] {
    mac(key, CONFIRM_A_LABEL, binding)
}

/// The backend's confirmation: a MAC under `key` over `binding`, under another
/// label than [`confirm_a`]'s.
pub fn confirm_b(key: &SharedKey, binding: &Binding) -> [u8; CONFIRM_LEN] {
    mac(key, CONFIRM_B_LABEL, binding)
}

/// Whether `presented` is the app's confirmation under `key` over `binding`,
/// compared in constant time.
pub fn check_confirm_a(key: &SharedKey, binding: &Binding, presented: &[u8]) -> bool {
    constant_time_eq(&confirm_a(key, binding), presented)
}

/// Whether `presented` is the backend's confirmation under `key` over
/// `binding`, compared in constant time.
pub fn check_confirm_b(key: &SharedKey, binding: &Binding, presented: &[u8]) -> bool {
    constant_time_eq(&confirm_b(key, binding), presented)
}

/// Whether `a` and `b` are equal, without an early exit on the first differing
/// byte.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // The length is not a secret; only the contents are compared blindly.
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Lower-case hex of `bytes`, the wire form of every field of the exchange.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The bytes of `hex`, when it is hex for exactly `len` bytes; `None`
/// otherwise.
pub fn from_hex(hex: &str, len: usize) -> Option<Vec<u8>> {
    if len == 0 || hex.len() != len * 2 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests;
