// SPDX-License-Identifier: MIT OR Apache-2.0

//! The one listener (#195): each connection is dispatched on its first byte,
//! a TLS handshake to the full router and anything else to the plain router.
//! Also the debug-only plain mode (`BLUE2TH_PLAIN_HTTP`).

use std::future::Future;
use std::time::Duration;

use axum::{
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    Json, Router,
};
use blue2th_proto::HealthStatus;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
    service::TowerToHyperService,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
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
        first_byte: Duration::from_secs(10),
        handshake: Duration::from_secs(10),
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
///
/// Only `1` in a debug build enables the plain mode. Anything else that was
/// set is ignored with a warning, so a typo never silently serves TLS while
/// the operator believes otherwise; an empty value is unset.
pub fn plain_http_choice(env: Option<&str>, debug: bool) -> PlainChoice {
    let warning = match env {
        None | Some("") => None,
        Some("1") if debug => {
            return PlainChoice {
                enabled: true,
                warning: None,
            }
        },
        Some("1") => Some(format!(
            "{PLAIN_HTTP_ENV} is read only by debug builds and was ignored; serving TLS"
        )),
        Some(other) => Some(format!(
            "{PLAIN_HTTP_ENV}={other:?} was ignored: only `1` turns the plain mode on, \
             in a debug build; serving TLS"
        )),
    };
    PlainChoice {
        enabled: false,
        warning,
    }
}

/// The address the plain mode binds with no `--bind` and no `BLUE2TH_BIND`.
const PLAIN_DEFAULT_BIND: &str = "127.0.0.1:4000";

/// The address the plain mode binds, from `--bind` (`cli`) and
/// `BLUE2TH_BIND` (`env_bind`), or the refusal.
///
/// Only an IP literal that is loopback is accepted: the token travels in clear
/// in this mode, so it must never leave the machine. A host name such as
/// `localhost` is refused too, since what it resolves to is not ours to know.
pub fn plain_bind(cli: Option<&str>, env_bind: Option<&str>) -> Result<String, String> {
    let chosen = cli
        .filter(|a| !a.is_empty())
        .or(env_bind.filter(|a| !a.is_empty()))
        .unwrap_or(PLAIN_DEFAULT_BIND);
    match chosen.parse::<std::net::SocketAddr>() {
        Ok(addr) if addr.ip().is_loopback() => Ok(chosen.to_string()),
        _ => Err(format!(
            "refusing to start: {PLAIN_HTTP_ENV} serves plain HTTP on a loopback IP \
             address only, and {chosen} is not one (use 127.0.0.1:<port> or [::1]:<port>)"
        )),
    }
}

/// The router the plain side of the listener serves.
///
/// It holds no API handler: a request under `/api` is answered `426` without
/// its token being read, so a command sent in clear is never run.
pub fn plain_router() -> Router {
    Router::new().fallback(plain_answer)
}

/// The one handler of [`plain_router`].
async fn plain_answer(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path();
    if method != Method::GET || path == "/api" || path.starts_with("/api/") {
        return StatusCode::UPGRADE_REQUIRED.into_response();
    }
    if path == "/health" {
        // The release version stays off the clear side; the protocol range is
        // what an app paired over HTTP needs to say "update the app".
        return Json(HealthStatus::ok(REDUCED_HEALTH_VERSION).with_auth_required(true))
            .into_response();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|h| !h.is_empty());
    let Some(host) = host else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let target = uri.path_and_query().map_or(path, |pq| pq.as_str());
    Redirect::permanent(&format!("https://{host}{target}")).into_response()
}

/// The version the plain `/health` announces in place of the release one.
const REDUCED_HEALTH_VERSION: &str = "0.0.0";

/// The first byte of a TLS handshake record (RFC 8446 §5.1, `handshake(22)`).
const TLS_HANDSHAKE: u8 = 0x16;

/// Accept on `listener` until `shutdown` resolves, dispatching each connection
/// on its first byte.
///
/// No graceful drain: the SSE streams never end, so the open connections are
/// left to the runtime once `shutdown` resolves and the socket is closed.
pub async fn serve<F>(
    listener: TcpListener,
    tls_router: Router,
    plain_router: Router,
    acceptor: TlsAcceptor,
    limits: Limits,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return Ok(()),
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    // Each connection owns a handle on the routers and the
                    // acceptor; all three are cheap, shared clones.
                    let tls_router = tls_router.clone();
                    let plain_router = plain_router.clone();
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        dispatch(stream, tls_router, plain_router, acceptor, limits).await;
                        tracing::trace!("connection from {peer} closed");
                    });
                },
                Err(e) => {
                    // A failed accept (e.g. out of file descriptors) concerns
                    // one connection; the pause keeps it from spinning.
                    tracing::warn!("accept failed: {e}");
                    tokio::time::sleep(ACCEPT_RETRY).await;
                },
            },
        }
    }
}

/// The pause after a failed accept.
const ACCEPT_RETRY: Duration = Duration::from_millis(50);

/// Serve one connection: TLS to the full router when its first byte opens a
/// handshake, plain HTTP to the plain router otherwise.
async fn dispatch(
    stream: TcpStream,
    tls_router: Router,
    plain_router: Router,
    acceptor: TlsAcceptor,
    limits: Limits,
) {
    let mut first = [0_u8; 1];
    match timeout(limits.first_byte, stream.peek(&mut first)).await {
        Ok(Ok(1..)) => {},
        Ok(Ok(_)) => return,
        Ok(Err(e)) => {
            tracing::debug!("could not read a first byte: {e}");
            return;
        },
        Err(_) => {
            tracing::debug!("no first byte within {:?}: closed", limits.first_byte);
            return;
        },
    }
    if first[0] != TLS_HANDSHAKE {
        serve_http(stream, plain_router).await;
        return;
    }
    match timeout(limits.handshake, acceptor.accept(stream)).await {
        Ok(Ok(tls)) => serve_http(tls, tls_router).await,
        // A scanner fails handshakes all day: never above `debug`.
        Ok(Err(e)) => tracing::debug!("TLS handshake failed: {e}"),
        Err(_) => tracing::debug!(
            "TLS handshake not done within {:?}: closed",
            limits.handshake
        ),
    }
}

/// Serve HTTP/1.1 or h2 on `io` with `router` until the connection ends.
async fn serve_http<I>(io: I, router: Router)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = TowerToHyperService::new(router);
    if let Err(e) = Builder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(TokioIo::new(io), service)
        .await
    {
        tracing::debug!("connection ended with an error: {e}");
    }
}

#[cfg(test)]
mod tests;
