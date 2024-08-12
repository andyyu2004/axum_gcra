axum_gcra
=========

GCRA-based Rate Limiter for Axum with per-route/per-key rate limiting.

[![crates.io](https://img.shields.io/crates/v/axum_gcra.svg)](https://crates.io/crates/axum_gcra)
[![Documentation](https://docs.rs/axum_gcra/badge.svg)](https://docs.rs/axum_gcra)
[![MIT/Apache-2 licensed](https://img.shields.io/crates/l/axum_gcra.svg)](./LICENSE-Apache)

# Summary

This crate provides a robust implementation of a rate-limiting `Layer` and `Service` for `axum` using the
GCRA algorithm, which allows for average throughput and burst request limiting. Rather than providing a global
rate-limiter for all routes, this crate allows for individual routes/paths to be configured with separate
rate-limiting quotas and for the extraction of arbitrary keys from the request for additional compartmentalization.

For example:
```rust,no_run
use std::time::Duration;

use axum::{routing::get, Router, http::Method};
use axum_gcra::{gcra::Quota, RateLimitLayer, real_ip::RealIp};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .route("/hello", get(|| async { "Hello, Again!" }))
        .route_layer(
            RateLimitLayer::<RealIp>::builder()
                .with_default_quota(Quota::simple(Duration::from_secs(5)))
                .with_route((Method::GET, "/"), Quota::simple(Duration::from_secs(1)))
                .with_route((Method::GET, "/hello"), Quota::simple(Duration::from_secs(2)))
                .with_global_fallback(true)
                .with_extension(true)
                .default_handle_error(),
        );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

# Keys

In order for the rate limiter to be able to differentiate between different clients, the `RateLimitLayer` can be
configured with single or composite keys that are extracted from the request. For example, to rate limit based on
the client's IP address, the following could be used with the provided `RealIp` extractor:

```rust,no_run
use axum::{routing::get, Router, extract::Extension};
use axum_gcra::{RateLimitLayer, real_ip::RealIp, extensions::RateLimiter};

type Key = RealIp; // could also be `(RealIp, UserSession)`, for example.

let app = Router::<()>::new()
    // keys are any type that can implement `FromRequestParts` and `axum_gcra::Key`
    .route("/", get(|ip: RealIp| async move { format!("Hello, {ip}!") }))
    // The key type must also be specified when extracting the `RateLimiter` extension.
    .route("/extension", get(|rl: Extension<RateLimiter<Key>>| async move {
        // do something with the rate limiter, etc.
        "Hello, Extensions!"
    }))
    .route_layer(RateLimitLayer::<Key>::builder().default_handle_error());
```

It's notable that `RealIp` will only be able to extract the client IP address from the underlying socket
if used with [`Router::into_make_service_with_connect_info`](axum::Router::into_make_service_with_connect_info),
otherwise it must only rely on the request headers. However, the socket address may not always be the client's
real IP address, especially if the server is behind a reverse proxy, so be sure to configure the reverse proxy
to forward the client's IP address in the `X-Forwarded-For` header, or one of the others that `RealIp` can extract.

Please read the documentation for [`RealIp`] for more information.

# Garbage Collection

Internally, the rate limiter uses a shared hash map structure to store the state of each key. To avoid
it growing indefinitely, a garbage collection mechanism is provided that will remove entries that have
expired and no longer needed. This can be configured to run based on the number of
requests processed or on a fixed time interval in a background task. For example:

```rust,no_run
use std::time::Duration;
use axum::{routing::get, Router};
use axum_gcra::{RateLimitLayer, real_ip::RealIp};

let app = Router::<()>::new()
    .route("/", get(|| async { "Hello, World!" }))
    .route_layer(
        RateLimitLayer::<RealIp>::builder()
            .with_gc_interval(1000) // run GC on every 1000th request
            .with_gc_interval(Duration::from_secs(60)) // or run every 60 seconds
            .default_handle_error(),
    );
```

See the docs for [`GCInterval`] for more information.

# Cargo Feature Flags

The follow features are enabled by default but can be disabled if not needed:

- `ahash`: Use the [`ahash`] crate for faster hashing of keys.
- `tokio`: Use the [`tokio`] crate for time-based GC intervals and specific socket utilities.
- `real_ip`: Enable the [`RealIp`] extractor, also enables the `tokio` feature.
- `itoa`: Use the [`itoa`] crate for integer to string conversion.
