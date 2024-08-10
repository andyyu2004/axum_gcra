#[cfg(not(feature = "tokio"))]
compile_error!("This example requires the 'tokio' feature to be enabled.");

use std::time::Duration;

use axum::{
    extract::Extension,
    routing::{get, post, Router},
};
use axum_gcra::{gcra::Quota, RateLimitLayer, RateLimiter};
use http::Method;

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .route("/build", get(|| async { "Build info" }))
        .route(
            "/penalize",
            get(|rl: Extension<RateLimiter>| async move {
                rl.penalize(Duration::from_secs(50)).await;

                "Penalized"
            }),
        )
        .route(
            "/reset",
            post(|rl: Extension<RateLimiter>| async move {
                rl.reset().await;

                "Reset"
            }),
        )
        .route_layer(
            // NOTE: A macro could be used to simplify this
            RateLimitLayer::builder()
                .set_default_quota(Quota::simple(Duration::from_secs(5)))
                .with_quota((Method::GET, "/build"), Quota::simple(Duration::from_secs(2)))
                .with_quota((Method::POST, "/reset"), Quota::simple(Duration::from_millis(1)))
                .with_extension(true) // required for the `Extension` extractor to work
                .set_global_fallback(true)
                .set_gc_interval(Duration::from_secs(5))
                .default_handle_error(),
        );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(async { _ = tokio::signal::ctrl_c().await })
        .await
        .unwrap();
}
