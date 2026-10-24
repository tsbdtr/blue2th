// SPDX-License-Identifier: MIT OR Apache-2.0

//! The one listener (#195): each connection is dispatched on its first byte,
//! a TLS handshake to the full router and anything else to the plain router.
//! Also the debug-only plain mode (`BLUE2TH_PLAIN_HTTP`).

use std::future::Future;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// The variable that turns the plain, loopback-only mode on in a debug build.
pub const PLAIN_HTTP_ENV: &str = "BLUE2TH_PLAIN_HTTP";

/// How long a connection may take at each step before it is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// From accept to the first byte.
    pub first_byte: Duration,
    /// From the first byte to the end of the TLS handshake.
    pub handshake: Duration,
}

impl Limits {
    /// The limits the backend runs with.
    pub const PRODUCTION: Limits = Limits {
        first_byte: Duration::ZERO,
        handshake: Duration::ZERO,
    };
}

/// Whether the plain mode is on, and what to log about the variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlainChoice {
    /// Whether the backend serves plain HTTP only.
    pub enabled: bool,
    /// A warning about a value that was ignored.
    pub warning: Option<String>,
}

/// Read `BLUE2TH_PLAIN_HTTP` (`env`) for a debug or a release build.
pub fn plain_http_choice(_env: Option<&str>, _debug: bool) -> PlainChoice {
    PlainChoice {
        enabled: false,
        warning: None,
    }
}

/// The address the plain mode binds, from `--bind` (`cli`) and
/// `BLUE2TH_BIND` (`env_bind`), or the refusal.
pub fn plain_bind(_cli: Option<&str>, _env_bind: Option<&str>) -> Result<String, String> {
    Ok(String::new())
}

/// The router the plain side of the listener serves.
pub fn plain_router() -> Router {
    Router::new()
}

/// Accept on `listener` until `shutdown` resolves, dispatching each connection
/// on its first byte.
pub async fn serve<F>(
    _listener: TcpListener,
    _tls_router: Router,
    _plain_router: Router,
    _acceptor: TlsAcceptor,
    _limits: Limits,
    _shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    Ok(())
}

#[cfg(test)]
mod tests;
