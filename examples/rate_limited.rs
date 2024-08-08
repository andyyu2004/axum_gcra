use std::time::Duration;

use axum::{routing::get, Router};
use axum_gcra::{gcra::Quota, RateLimitLayer};

#[tokio::main]
async fn main() {
    // build our application with a single route
    let app = Router::new().route("/", get(|| async { "Hello, World!" })).route_layer(
        RateLimitLayer::builder()
            .set_default_quota(Quota::simple(Duration::from_secs(5)))
            .with_extension(true)
            .default_handle_error(),
    );

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
