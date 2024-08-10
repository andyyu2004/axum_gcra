#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_auto_cfg, doc_cfg))]

use std::{
    borrow::Cow,
    collections::HashMap,
    convert::Infallible,
    future::{Future, Ready},
    hash::Hash,
    ops::Deref,
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
    time::{Duration, Instant},
};

use axum::{
    error_handling::HandleErrorLayer,
    extract::{MatchedPath as AxumMatchedPath, Request},
};
use http::{request::Parts, Extensions, Method};
use tower::{Layer, Service};

pub mod gcra;
pub use gcra::RateLimitError;

/// Interval for garbage collection of the rate limiter, which can be either
/// a number of requests or a time duration.
///
/// The default is 8192 requests.
///
/// Time durations require the `tokio` cargo feature to be enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GCInterval {
    /// Run garbage collection after a number of requests.
    Requests(u64),

    /// Run garbage collection on a timed interval using a background task.
    #[cfg(feature = "tokio")]
    Time(Duration),
}

impl Default for GCInterval {
    fn default() -> Self {
        GCInterval::Requests(8192)
    }
}

impl GCInterval {
    fn to_requests(self) -> u64 {
        match self {
            GCInterval::Requests(n) => n,

            #[cfg(feature = "tokio")]
            GCInterval::Time(_) => u64::MAX,
        }
    }
}

impl From<u64> for GCInterval {
    fn from(n: u64) -> Self {
        GCInterval::Requests(n)
    }
}

#[cfg(feature = "tokio")]
impl From<Duration> for GCInterval {
    fn from(d: Duration) -> Self {
        GCInterval::Time(d)
    }
}

/// A route for rate limiting, consisting of a path and method.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct Route<'a> {
    pub method: Cow<'a, Method>,
    pub path: Cow<'a, str>,
}

impl<'a, S> From<(Method, S)> for Route<'a>
where
    S: Into<Cow<'a, str>>,
{
    fn from((method, path): (Method, S)) -> Self {
        Self {
            path: path.into(),
            method: Cow::Owned(method),
        }
    }
}

macro_rules! decl_route_methods {
    ($($fn:ident => $method:ident),*) => {
        impl<'a> Route<'a> {
            /// Create a new route with the given method and path.
            pub fn new(method: Method, path: impl Into<Cow<'a, str>>) -> Self {
                Route {
                    method: Cow::Owned(method),
                    path: path.into(),
                }
            }

            $(
                #[doc = concat!("Create a new route with the [`", stringify!($method), "`](Method::", stringify!($method), ") method.")]
                pub fn $fn(path: impl Into<Cow<'a, str>>) -> Route<'a> {
                    Route::new(Method::$method, path)
                }
            )*
        }
    };
}

decl_route_methods! {
    get     => GET,
    post    => POST,
    put     => PUT,
    delete  => DELETE,
    patch   => PATCH,
    options => OPTIONS,
    head    => HEAD,
    trace   => TRACE,
    connect => CONNECT
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct RouteWithKey<T> {
    path: MatchedPath,
    method: Method,
    key: T,
}

/// Hashmap of quotas for rate limiting, mapping a path as passed to [`Router`](axum::Router) to a [`gcra::Quota`].
type Quotas = HashMap<Route<'static>, gcra::Quota, ahash::RandomState>;

#[derive(Debug, Clone)]
enum MatchedPath {
    Fallback,
    Axum(AxumMatchedPath),
}

impl Deref for MatchedPath {
    type Target = str;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        match self {
            MatchedPath::Fallback => "",
            MatchedPath::Axum(path) => path.as_str(),
        }
    }
}

impl PartialEq for MatchedPath {
    fn eq(&self, other: &Self) -> bool {
        let a = &**self;
        let b = &**other;

        // compare Arc pointers first
        a.as_ptr() == b.as_ptr() || a == b
    }
}

impl Eq for MatchedPath {}

impl Hash for MatchedPath {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

/// Defines a trait for extracting a key from a request.
///
/// This is similar to [`FromRequestParts`](axum::extract::FromRequestParts), but is always
/// infallible and synchronous.
pub trait GetKey {
    /// The type of the key extracted from the request.
    type T: Hash + Eq + Send + Sync + 'static;

    /// Extract a key from the request.
    ///
    /// For example, this could be the IP address of the client, or a user/session ID, or
    /// any other value that uniquely identifies the client.
    fn get_key(&self, req: &Parts) -> Self::T;
}

impl GetKey for () {
    type T = ();

    #[inline]
    fn get_key(&self, _: &Parts) {}
}

impl<F, T> GetKey for F
where
    F: Fn(&Parts) -> T,
    T: Hash + Eq + Send + Sync + 'static,
{
    type T = T;

    #[inline]
    fn get_key(&self, req: &Parts) -> T {
        self(req)
    }
}

/// Rate limiter [`Service`] for axum.
///
/// This struct is not meant to be used directly, but rather through the [`RateLimitLayerBuilder`].
///
/// Note: The limiter is shared across all clones of the layer and service.
pub struct RateLimitService<S, K: GetKey = ()> {
    inner: S,
    layer: RateLimitLayer<K>,
}

#[cfg(feature = "tokio")]
#[derive(Default, Clone)]
struct BuilderDropNotify {
    notify: Arc<tokio::sync::Notify>,
}

/// Builder for the rate limiter layer.
///
/// This struct is used to configure the rate limiter before building it.
pub struct RateLimitLayerBuilder<K: GetKey = ()> {
    quotas: Quotas,
    default_quota: gcra::Quota,
    set_ext: Option<Box<dyn SetExtension<K::T>>>,
    global_fallback: bool,
    get_key: K,
    gc_interval: GCInterval,

    #[cfg(feature = "tokio")]
    shutdown: BuilderDropNotify,
}

impl<K: GetKey> Drop for RateLimitLayerBuilder<K> {
    fn drop(&mut self) {
        #[cfg(feature = "tokio")]
        self.shutdown.notify.notify_waiters();
    }
}

/// Rate limiter [`Layer`] for axum.
///
/// This struct is not meant to be used directly, but rather through the [`RateLimitLayerBuilder`].
///
/// Note: The limiter is shared across all clones of the layer and service.
pub struct RateLimitLayer<K: GetKey = ()> {
    builder: Arc<RateLimitLayerBuilder<K>>,
    limiter: Arc<gcra::RateLimiter<RouteWithKey<K::T>, ahash::RandomState>>,
}

/// Object-safe trait for setting an extension on a request.
///
/// Used to insert the rate limiter into the request's extensions,
/// without knowing the type of the key except when the handler is defined and not further.
trait SetExtension<T: Hash + Eq>: Send + Sync + 'static {
    fn set_extension(
        &self,
        req: &mut Extensions,
        key: &RouteWithKey<T>,
        limiter: Arc<gcra::RateLimiter<RouteWithKey<T>, ahash::RandomState>>,
    );
}

struct DoSetExtension;

impl<T: Hash + Eq> SetExtension<T> for DoSetExtension
where
    T: Clone + Send + Sync + 'static,
{
    fn set_extension(
        &self,
        req: &mut Extensions,
        key: &RouteWithKey<T>,
        limiter: Arc<gcra::RateLimiter<RouteWithKey<T>, ahash::RandomState>>,
    ) {
        req.insert(RateLimiter {
            key: key.clone(),
            limiter,
        });
    }
}

impl<K: GetKey> Clone for RateLimitLayer<K> {
    fn clone(&self) -> Self {
        Self {
            limiter: self.limiter.clone(),
            builder: self.builder.clone(),
        }
    }
}

impl<S: Clone, K: GetKey> Clone for RateLimitService<S, K> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            layer: self.layer.clone(),
        }
    }
}

impl<S, K: GetKey> RateLimitService<S, K> {
    fn req_sync_peek_key<F>(
        &self,
        mut key: RouteWithKey<K::T>,
        now: std::time::Instant,
        peek: F,
    ) -> Result<(), RateLimitError>
    where
        F: FnOnce(&RouteWithKey<K::T>),
    {
        let builder = &self.layer.builder;

        let quota_key = Route {
            path: Cow::Borrowed(&*key.path),
            method: Cow::Borrowed(&key.method),
        };

        let quota = match builder.quotas.get(&quota_key).copied() {
            Some(quota) => quota,
            None => {
                if builder.global_fallback {
                    key.path = MatchedPath::Fallback;
                }

                builder.default_quota
            }
        };

        self.layer.limiter.req_sync_peek_key(key, quota, now, peek)
    }
}

impl<K: GetKey> RateLimitLayer<K> {
    /// Begin building a new rate limiter layer starting with a key extractor.
    pub fn builder_with_key(get_key: K) -> RateLimitLayerBuilder<K> {
        RateLimitLayerBuilder {
            quotas: Default::default(),
            default_quota: Default::default(),
            set_ext: None,
            global_fallback: false,
            get_key,
            gc_interval: GCInterval::default(),

            #[cfg(feature = "tokio")]
            shutdown: BuilderDropNotify::default(),
        }
    }
}

impl RateLimitLayer<()> {
    /// Begin building a new rate limiter layer starting with the default configuration.
    pub fn builder() -> RateLimitLayerBuilder<()> {
        Self::builder_with_key(())
    }
}

impl<K: GetKey> RateLimitLayerBuilder<K> {
    /// Insert a route entry into the quota table for the rate limiter.
    pub fn add_quota(&mut self, route: impl Into<Route<'static>>, quota: gcra::Quota) {
        self.add_quotas(Some((route.into(), quota)));
    }

    /// Insert a route entry into the quota table for the rate limiter.
    pub fn with_quota(mut self, route: impl Into<Route<'static>>, quota: gcra::Quota) -> Self {
        self.add_quota(route.into(), quota);
        self
    }

    /// Insert many route entries into the quota table for the rate limiter.
    pub fn add_quotas(&mut self, quotas: impl IntoIterator<Item = (impl Into<Route<'static>>, gcra::Quota)>) {
        self.quotas.extend(quotas.into_iter().map(|(route, quota)| (route.into(), quota)));
    }

    /// Insert many route entries into the quota table for the rate limiter.
    pub fn with_quotas(
        mut self,
        quotas: impl IntoIterator<Item = (impl Into<Route<'static>>, gcra::Quota)>,
    ) -> Self {
        self.add_quotas(quotas);
        self
    }

    /// Set the quota table for the rate limiter, replacing any existing routes and quotas.
    pub fn set_quotas(
        mut self,
        quotas: impl IntoIterator<Item = (impl Into<Route<'static>>, gcra::Quota)>,
    ) -> Self {
        self.quotas.clear();
        self.add_quotas(quotas);
        self
    }

    /// Fallback quota for rate limiting if no specific quota is found for the path.
    pub fn set_default_quota(mut self, default_quota: gcra::Quota) -> Self {
        self.default_quota = default_quota;
        self
    }

    /// Set whether to use a global fallback shared rate-limiter for all paths not explicitly defined.
    pub fn set_global_fallback(mut self, global_fallback: bool) -> Self {
        self.global_fallback = global_fallback;
        self
    }

    /// Set the interval for which garbage collection for the rate limiter will occur.
    /// Garbage collection in this case is defined as removing old requests from the rate limiter.
    ///
    /// The default is 8192 requests.
    ///
    /// If the `tokio` feature is enabled, this can also be a time [`Duration`],
    /// and a background task will be spawned to clean the rate limiter at the given interval.
    pub fn set_gc_interval(mut self, gc_interval: impl Into<GCInterval>) -> Self {
        self.gc_interval = gc_interval.into();
        self
    }

    /// Provide a function to extract a key from the request.
    pub fn with_key<K2>(mut self, get_key: K2) -> RateLimitLayerBuilder<K2>
    where
        K2: GetKey,
    {
        RateLimitLayerBuilder {
            quotas: std::mem::take(&mut self.quotas),
            default_quota: self.default_quota,
            set_ext: None,
            global_fallback: self.global_fallback,
            get_key,
            gc_interval: self.gc_interval,

            #[cfg(feature = "tokio")]
            shutdown: std::mem::take(&mut self.shutdown),
        }
    }

    /// Set whether to insert the [`RateLimiter`] into the request's extensions
    /// to allow for manual rate limiting control.
    pub fn with_extension(mut self, extend: bool) -> Self
    where
        K::T: Clone + Send + Sync + 'static,
    {
        self.set_ext = match extend {
            true => Some(Box::new(DoSetExtension)),
            false => None,
        };
        self
    }
}

impl Default for RateLimitLayerBuilder<()> {
    fn default() -> Self {
        RateLimitLayer::builder()
    }
}

/// Error wrapper for rate limiting errors or inner service errors.
#[derive(Debug)]
pub enum Error<Inner> {
    /// Inner service error.
    ///
    /// For most axum services, this will be a [`Infallible`].
    Inner(Inner),

    /// Rate limiting error.
    ///
    /// This error is returned when the rate limiter has blocked the request,
    /// and will be passed to the [error handler](RateLimitLayerBuilder::handle_error).
    RateLimit(RateLimitError),
}

use futures_util::TryFuture;

pin_project_lite::pin_project! {
    #[doc(hidden)]
    #[project = RateLimitedResponseProj]
    pub enum RateLimitedResponse<F> {
        Ok { #[pin] f: F },
        Err { e: RateLimitError },
    }
}

impl<F> Future for RateLimitedResponse<F>
where
    F: TryFuture,
{
    type Output = Result<F::Ok, Error<F::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            RateLimitedResponseProj::Ok { f } => match ready!(f.try_poll(cx)) {
                Ok(ok) => Poll::Ready(Ok(ok)),
                Err(e) => Poll::Ready(Err(Error::Inner(e))),
            },
            RateLimitedResponseProj::Err { e } => Poll::Ready(Err(Error::RateLimit(*e))),
        }
    }
}

impl<S, K, B> Service<Request<B>> for RateLimitService<S, K>
where
    S: Service<Request<B>, Future: TryFuture<Ok = S::Response, Error = S::Error>>,
    K: GetKey,
{
    type Response = S::Response;
    type Error = Error<S::Error>;
    type Future = RateLimitedResponse<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.inner.poll_ready(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(Error::Inner(e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let now = std::time::Instant::now();

        let path = match req.extensions().get::<AxumMatchedPath>() {
            Some(path) => MatchedPath::Axum(path.clone()),
            None => MatchedPath::Fallback,
        };

        let builder = &self.layer.builder;

        let (mut parts, body) = req.into_parts();

        let key = RouteWithKey {
            path,
            method: parts.method.clone(),
            key: builder.get_key.get_key(&parts),
        };

        let res = self.req_sync_peek_key(key, now, |key| {
            if let Some(ref set_ext) = builder.set_ext {
                // set_extension will clone the key internally
                set_ext.set_extension(&mut parts.extensions, key, self.layer.limiter.clone());
            }
        });

        match res {
            Err(e) => RateLimitedResponse::Err { e },
            Ok(()) => RateLimitedResponse::Ok {
                f: self.inner.call(Request::from_parts(parts, body)),
            },
        }
    }
}

impl<K, S> Layer<S> for RateLimitLayer<K>
where
    K: GetKey,
{
    type Service = RateLimitService<S, K>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            layer: self.clone(),
        }
    }
}

use tower::layer::util::Stack;

impl<K> RateLimitLayerBuilder<K>
where
    K: GetKey,
{
    /// Build the [`RateLimitLayer`].
    ///
    /// This will create a new rate limiter and, if the `tokio` feature is
    /// enabled and the interval is a time [`Duration`], spawn a background task for
    /// garbage collection.
    ///
    /// By itself, `RateLimitLayer` cannot be directly inserted into an [`axum::Router`],
    /// as it requires a [`HandleErrorLayer`] to handle rate limiting errors.
    /// Use [`RateLimitLayerBuilder::handle_error`] or [`RateLimitLayerBuilder::default_handle_error`] to create a stack
    /// with the rate limiter layer and the error-handler layer combined.
    pub fn build(self) -> RateLimitLayer<K> {
        let limiter = Arc::new(gcra::RateLimiter::new(
            self.gc_interval.to_requests(),
            Default::default(),
        ));

        #[cfg(feature = "tokio")]
        if let GCInterval::Time(d) = self.gc_interval {
            let limiter = limiter.clone();
            let signal = self.shutdown.clone();

            _ = tokio::task::spawn(async move {
                let mut interval = tokio::time::interval(d);
                loop {
                    tokio::select! { biased;
                        _ = signal.notify.notified() => break,
                        _ = interval.tick() => {},
                    }

                    limiter.clean(Instant::now()).await;

                    // also close task if no more references to the limiter
                    if Arc::strong_count(&limiter) == 1 {
                        break;
                    }
                }
            });
        }

        RateLimitLayer {
            limiter,
            builder: Arc::new(self),
        }
    }

    /// Create a new rate limiter layer with the provided error-handler callback.
    ///
    /// Returns a [`Stack`]-ed layer with the rate limiter layer and the error-handler layer combined
    /// that can be directly inserted into an [`axum::Router`].
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use axum::{Router, http::StatusCode};
    /// use axum_gcra::RateLimitLayer;
    ///
    /// let builder = RateLimitLayer::builder();
    ///
    /// let app = Router::<()>::new().route_layer(
    ///    builder.handle_error(|e| async move {
    ///       (StatusCode::TOO_MANY_REQUESTS, e.to_string())
    ///    }));
    /// ```
    pub fn handle_error<F, R>(
        self,
        cb: F,
    ) -> Stack<RateLimitLayer<K>, HandleErrorLayer<impl Fn(Error<Infallible>) -> R + Clone, ()>>
    where
        F: Fn(RateLimitError) -> R + Clone,
    {
        Stack::new(
            self.build(),
            HandleErrorLayer::new(move |e| match e {
                Error::RateLimit(e) => cb(e),
                Error::Inner(_) => unsafe { core::hint::unreachable_unchecked() },
            }),
        )
    }

    /// Create a new rate limiter layer with the default error-handler callback that simply returns the error.
    ///
    /// Returns a [`Stack`]-ed layer with the rate limiter layer and the error-handler layer combined
    /// that can be directly inserted into an [`axum::Router`].
    pub fn default_handle_error(
        self,
    ) -> Stack<RateLimitLayer<K>, HandleErrorLayer<impl Fn(Error<Infallible>) -> Ready<RateLimitError> + Clone, ()>>
    {
        self.handle_error(std::future::ready)
    }
}

/// [`Request`] extension to access the internal rate limiter used during that request,
/// such as to apply a penalty or reset the rate limit.
#[derive(Clone)]
pub struct RateLimiter<T: Hash + Eq = ()> {
    key: RouteWithKey<T>,
    limiter: Arc<gcra::RateLimiter<RouteWithKey<T>, ahash::RandomState>>,
}

impl<T: Hash + Eq> RateLimiter<T> {
    /// See [`gcra::RateLimiter::penalize`] for more information.
    pub async fn penalize(&self, penalty: Duration) -> bool {
        self.limiter.penalize(&self.key, penalty).await
    }

    /// See [`gcra::RateLimiter::penalize_sync`] for more information.
    pub fn penalize_sync(&self, penalty: Duration) -> bool {
        self.limiter.penalize_sync(&self.key, penalty)
    }

    /// See [`gcra::RateLimiter::reset`] for more information.
    pub async fn reset(&self) -> bool {
        self.limiter.reset(&self.key).await
    }

    /// See [`gcra::RateLimiter::reset_sync`] for more information.
    pub fn reset_sync(&self) -> bool {
        self.limiter.reset_sync(&self.key)
    }

    /// See [`gcra::RateLimiter::clean`] for more information.
    pub async fn clean(&self, before: Instant) {
        self.limiter.clean(before).await;
    }

    /// See [`gcra::RateLimiter::clean_sync`] for more information.
    pub fn clean_sync(&self, before: Instant) {
        self.limiter.clean_sync(before);
    }
}
