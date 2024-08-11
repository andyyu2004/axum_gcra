//! Provides a way to get the real IP address of the client, to the best of our ability.
//!
//! This is exposed as [`RealIp`], which is an extractor that can be used in Axum routes,
//! or as a [`Layer`] and [`Service`] that can be used to add the IP address to
//! the request parts extensions. Or both.

use std::{
    fmt::{self, Debug, Display},
    hash::Hash,
    net::{IpAddr, SocketAddr},
    ops::Deref,
    str::FromStr,
    task::{Context, Poll},
};

use axum::{extract::FromRequestParts, response::IntoResponse};
use http::{header::HeaderName, request::Parts, HeaderValue, Request, StatusCode};
use tower::{Layer, Service};

/// Wrapper around `std::net::IpAddr` that can be extracted from the request parts.
///
/// This extractor will try to get the real IP address of the client, using the following headers, in order:
/// - `x-forwarded-for`
/// - `x-real-ip`
/// - `true-client-ip` (used by some load balancers)
/// - `client-ip` (used by some load balancers)
/// - `x-cluster-client-ip` (used by AWS sometimes)
/// - `cf-connecting-ip` (used by Cloudflare sometimes)
/// - `fly-client-ip` (used by Fly.io sometimes)
/// - `cloudfront-viewer-address` (used by Cloudfront sometimes)
///
/// If none of these headers are found, it will return a 400 Bad Request via [`IpAddrRejection`],
/// or the error can be handled with a custom rejection handler with
/// [`RateLimitLayerBuilder::handle_error`](crate::RateLimitLayerBuilder::handle_error).
///
/// If the `tokio` feature is enabled AND the `Router` is configured with
/// [`Router::into_make_service_with_connect_info`](axum::Router::into_make_service_with_connect_info),
/// it will also try to get the IP address from the underlying socket via the [`ConnectInfo<SocketAddr>`](axum::extract::ConnectInfo) extension.
/// This is optional as it may not work as expected if the server is behind a reverse proxy.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct RealIp(pub IpAddr);

impl Debug for RealIp {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

impl Display for RealIp {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl Deref for RealIp {
    type Target = IpAddr;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// IP Address not found, returns a 400 Bad Request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IpAddrRejection;

impl IntoResponse for IpAddrRejection {
    fn into_response(self) -> axum::response::Response {
        StatusCode::BAD_REQUEST.into_response()
    }
}

#[async_trait::async_trait]
impl<S> FromRequestParts<S> for RealIp {
    type Rejection = IpAddrRejection;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        if let Some(ip) = parts.extensions.get::<RealIp>() {
            return Ok(*ip);
        }

        match get_ip_from_parts(parts) {
            Some(ip) => Ok(RealIp(ip)),
            None => Err(IpAddrRejection),
        }
    }
}

/// [`Service`] that adds the [`RealIp`] extension to the request parts if available.
///
/// This extension can be reused by other services or extractors, such as [`RealIp`] itself.
#[derive(Debug, Clone, Copy)]
pub struct RealIpService<I>(I);

/// [`Layer`] that adds the [`RealIp`] extension to the request parts if available.
///
/// This extension can be reused by other services or extractors, such as [`RealIp`] itself.
#[derive(Debug, Clone, Copy)]
pub struct RealIpLayer;

impl<B, I> Service<Request<B>> for RealIpService<I>
where
    I: Service<Request<B>>,
{
    type Response = I::Response;
    type Error = I::Error;
    type Future = I::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let (mut parts, body) = req.into_parts();

        if let Some(ip) = get_ip_from_parts(&parts) {
            parts.extensions.insert(RealIp(ip));
        }

        self.0.call(Request::from_parts(parts, body))
    }
}

impl<I> Layer<I> for RealIpLayer {
    type Service = RealIpService<I>;

    fn layer(&self, inner: I) -> Self::Service {
        RealIpService(inner)
    }
}

fn get_ip_from_parts(parts: &Parts) -> Option<IpAddr> {
    fn parse_ip(s: &HeaderValue) -> Option<IpAddr> {
        s.to_str()
            .ok()
            .and_then(|s| s.split(&[',', ':']).next())
            .and_then(|s| IpAddr::from_str(s.trim()).ok())
    }

    static HEADERS: [HeaderName; 8] = [
        HeaderName::from_static("x-forwarded-for"),
        HeaderName::from_static("x-real-ip"),
        HeaderName::from_static("true-client-ip"), // used by some load balancers
        HeaderName::from_static("client-ip"),      // used by some load balancers
        HeaderName::from_static("x-cluster-client-ip"), // used by AWS sometimes
        HeaderName::from_static("cf-connecting-ip"), // used by Cloudflare sometimes
        HeaderName::from_static("fly-client-ip"),  // used by Fly.io sometimes
        HeaderName::from_static("cloudfront-viewer-address"), // used by Cloudfront sometimes
    ];

    for header in &HEADERS {
        if let Some(real_ip) = parts.headers.get(header).and_then(parse_ip) {
            return Some(real_ip);
        }
    }

    #[cfg(feature = "tokio")]
    if let Some(info) = parts.extensions.get::<axum::extract::ConnectInfo<SocketAddr>>() {
        return Some(info.ip());
    }

    None
}
