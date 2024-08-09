#[cfg(not(feature = "timed_gc"))]
compile_error!("This example requires the 'timed_gc' feature to be enabled.");

use std::time::Duration;

use axum::{extract::Request, routing::get, Router};
use axum_gcra::{gcra::Quota, RateLimitLayer, RateLimiter};
use http::Method;

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .route("/build", get(|| async { "Build info" }))
        .route(
            "/penalize",
            get(|req: Request| async move {
                let rl = req.extensions().get::<RateLimiter<()>>().unwrap();
                rl.penalize_sync(Duration::from_secs(50));

                "Penalized"
            }),
        )
        .route_layer(
            RateLimitLayer::builder()
                .set_default_quota(Quota::simple(Duration::from_secs(5)))
                .with_quota("/build", Method::GET, Quota::simple(Duration::from_secs(2)))
                .with_extension(true)
                .set_root_fallback(true)
                .set_gc_interval(Duration::from_secs(5))
                .default_handle_error(),
        );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(async { _ = tokio::signal::ctrl_c().await })
        .await
        .unwrap();
}
