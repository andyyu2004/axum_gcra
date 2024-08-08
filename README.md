axum_gcra
=========

GCRA-based Rate Limiter for Axum with per-route/per-key rate limiting

[![crates.io](https://img.shields.io/crates/v/axum_gcra.svg)](https://crates.io/crates/axum_gcra)
[![Documentation](https://docs.rs/axum_gcra/badge.svg)](https://docs.rs/axum_gcra)
[![MIT/Apache-2 licensed](https://img.shields.io/crates/l/axum_gcra.svg)](./LICENSE-Apache)

# Summary

This crate provides a robust implementation of a rate-limiting `Layer` and `Service` for `axum` using the
GCRA algorithm, which allows for average throughput and burst request limiting. Rather than providing a global
rate-limited for all routes, this crate allows for individual routes/paths to be configured with separate
rate-limiting quotas and for the extraction of arbitrary keys from the request for additional compartmentalization.

For example:
```rust,no_run
use std::time::Duration;

use axum::{routing::get, Router};
use axum_gcra::{gcra::Quota, RateLimitLayer};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .route_layer(
            RateLimitLayer::builder()
                .set_default_quota(Quota::simple(Duration::from_secs(5)))
                .with_extension(true)
                .default_handle_error(),
        );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```