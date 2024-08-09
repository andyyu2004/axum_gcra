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
use http::{Extensions, Method};
use tower::{Layer, Service};

pub mod gcra;
pub use gcra::RateLimitError;

/// Interval for garbage collection of the rate limiter, which can be either
/// a number of requests or a time duration.
///
/// The default is 8192 requests.
///
/// Time durations require the `timed_gc` cargo feature to be enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GCInterval {
    /// Run garbage collection after a number of requests.
    Requests(u64),

    /// Run garbage collection on a timed interval using a background task.
    #[cfg(feature = "timed_gc")]
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

            #[cfg(feature = "timed_gc")]
            GCInterval::Time(_) => u64::MAX,
        }
    }
}

impl From<u64> for GCInterval {
    fn from(n: u64) -> Self {
        GCInterval::Requests(n)
    }
}

#[cfg(feature = "timed_gc")]
impl From<Duration> for GCInterval {
    fn from(d: Duration) -> Self {
        GCInterval::Time(d)
    }
}

/// A route for rate limiting, consisting of a path and method.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct Route<'a> {
    pub path: Cow<'a, str>,
    pub method: Cow<'a, Method>,
}

macro_rules! decl_route_methods {
    ($($fn:ident => $method:ident),*) => {
        impl<'a> Route<'a> {
            /// Create a new route with the given path and method.
            pub fn new(path: impl Into<Cow<'a, str>>, method: Method) -> Self {
                Self {
                    path: path.into(),
                    method: Cow::Owned(method),
                }
            }

            $(
                #[doc = concat!("Create a new route with the [`", stringify!($method), "`](Method::", stringify!($method), ") method.")]
                pub fn $fn(path: impl Into<Cow<'a, str>>) -> Route<'a> {
                    Route::new(path, Method::$method)
                }
            )*
        }
    };
}

decl_route_methods! {
    get => GET,
    post => POST,
    put => PUT,
    delete => DELETE,
    patch => PATCH,
    options => OPTIONS,
    head => HEAD,
    trace => TRACE,
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
    Root,
    Axum(AxumMatchedPath),
}

impl Deref for MatchedPath {
    type Target = str;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        match self {
            MatchedPath::Root => "/",
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
pub trait GetKey<B>: Clone {
    type T: Hash + Eq + Send + Sync + 'static;

    fn get_key(&self, req: &Request<B>) -> Self::T;
}

/// Default implementation of [`GetKey`] that does nothing.
#[derive(Clone, Copy)]
pub struct DefaultGetKey;

impl<B> GetKey<B> for DefaultGetKey {
    type T = ();

    #[inline]
    fn get_key(&self, _: &Request<B>) {}
}

impl<F, T, B> GetKey<B> for F
where
    F: Clone + Fn(&Request<B>) -> T,
    T: Hash + Eq + Send + Sync + 'static,
{
    type T = T;

    #[inline]
    fn get_key(&self, req: &Request<B>) -> T {
        self(req)
    }
}

pub struct RateLimitService<S, K: GetKey<B>, B> {
    inner: S,
    layer: RateLimitLayer<K, B>,
}

#[cfg(feature = "timed_gc")]
#[derive(Default, Clone)]
struct BuilderDropNotify {
    notify: Arc<tokio::sync::Notify>,
}

pub struct RateLimitLayerBuilder<K: GetKey<B>, B> {
    quotas: Quotas,
    default_quota: gcra::Quota,
    set_ext: Option<Box<dyn SetExtension<K::T>>>,
    root_fallback: bool,
    get_key: K,
    gc_interval: GCInterval,

    #[cfg(feature = "timed_gc")]
    shutdown: BuilderDropNotify,
}

#[cfg(feature = "timed_gc")]
impl<K: GetKey<B>, B> Drop for RateLimitLayerBuilder<K, B> {
    fn drop(&mut self) {
        self.shutdown.notify.notify_waiters();
    }
}

pub struct RateLimitLayer<K: GetKey<B>, B> {
    builder: Arc<RateLimitLayerBuilder<K, B>>,
    limiter: Arc<gcra::RateLimiter<RouteWithKey<K::T>, ahash::RandomState>>,
}

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

impl<K: GetKey<B>, B> Clone for RateLimitLayer<K, B> {
    fn clone(&self) -> Self {
        Self {
            limiter: self.limiter.clone(),
            builder: self.builder.clone(),
        }
    }
}

impl<S: Clone, K: GetKey<B>, B> Clone for RateLimitService<S, K, B>
where
    K: GetKey<B>,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            layer: self.layer.clone(),
        }
    }
}

impl<S, K: GetKey<B>, B> RateLimitService<S, K, B> {
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
                if builder.root_fallback {
                    key.path = MatchedPath::Root;
                }

                builder.default_quota
            }
        };

        self.layer.limiter.req_sync_peek_key(key, quota, now, peek)
    }
}

impl<B> RateLimitLayer<DefaultGetKey, B> {
    /// Build a new rate limiter layer starting with the default configuration.
    pub fn builder() -> RateLimitLayerBuilder<DefaultGetKey, B> {
        RateLimitLayerBuilder {
            quotas: Default::default(),
            default_quota: Default::default(),
            set_ext: None,
            root_fallback: false,
            get_key: DefaultGetKey,
            gc_interval: GCInterval::default(),

            #[cfg(feature = "timed_gc")]
            shutdown: BuilderDropNotify::default(),
        }
    }
}

impl<B> RateLimitLayerBuilder<DefaultGetKey, B> {
    /// Insert a route entry into the quota table for the rate limiter.
    pub fn add_quota(&mut self, path: impl Into<Cow<'static, str>>, method: Method, quota: gcra::Quota) {
        self.add_quotas(Some((path.into(), method, quota)));
    }

    /// Insert a route entry into the quota table for the rate limiter.
    pub fn with_quota(mut self, path: impl Into<Cow<'static, str>>, method: Method, quota: gcra::Quota) -> Self {
        self.add_quota(path, method, quota);
        self
    }

    /// Insert many route entries into the quota table for the rate limiter.
    pub fn add_quotas(
        &mut self,
        quotas: impl IntoIterator<Item = (impl Into<Cow<'static, str>>, Method, gcra::Quota)>,
    ) {
        self.quotas
            .extend(quotas.into_iter().map(|(path, method, quota)| (Route::new(path, method), quota)));
    }

    /// Insert many route entries into the quota table for the rate limiter.
    pub fn with_quotas(
        mut self,
        quotas: impl IntoIterator<Item = (impl Into<Cow<'static, str>>, Method, gcra::Quota)>,
    ) -> Self {
        self.add_quotas(quotas);
        self
    }

    /// Set the quota table for the rate limiter, replacing any existing routes and quotas.
    pub fn set_quotas(
        mut self,
        quotas: impl IntoIterator<Item = (impl Into<Cow<'static, str>>, Method, gcra::Quota)>,
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

    /// Set whether to fallback to the root path (`/`) if no specific quota is found for the path.
    ///
    /// This allows for a single root quota to be used for all fallback paths.
    pub fn set_root_fallback(mut self, root_fallback: bool) -> Self {
        self.root_fallback = root_fallback;
        self
    }

    /// Set the interval for which garbage collection for the rate limiter will occur.
    /// Garbage collection in this case is defined as removing old requests from the rate limiter.
    ///
    /// The default is 8192 requests.
    ///
    /// If the `timed_gc` feature is enabled, this can also be a time [`Duration`],
    /// and a background task will be spawned to clean the rate limiter at the given interval.
    pub fn set_gc_interval(mut self, gc_interval: impl Into<GCInterval>) -> Self {
        self.gc_interval = gc_interval.into();
        self
    }

    /// Provide a function to extract a key from the request.
    pub fn with_key<K>(mut self, get_key: K) -> RateLimitLayerBuilder<K, B>
    where
        K: GetKey<B>,
    {
        RateLimitLayerBuilder {
            quotas: std::mem::take(&mut self.quotas),
            default_quota: self.default_quota,
            set_ext: None,
            root_fallback: self.root_fallback,
            get_key,
            gc_interval: self.gc_interval,

            #[cfg(feature = "timed_gc")]
            shutdown: std::mem::take(&mut self.shutdown),
        }
    }
}

impl<K: GetKey<B>, B> RateLimitLayerBuilder<K, B> {
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

impl Default for RateLimitLayerBuilder<DefaultGetKey, axum::body::Body> {
    fn default() -> Self {
        RateLimitLayer::builder()
    }
}

/// Error wrapper for rate limiting errors or inner service errors.
#[derive(Debug)]
pub enum Error<Inner> {
    /// Inner service error.
    Inner(Inner),

    /// Rate limiting error.
    RateLimit(RateLimitError),
}

use futures_util::TryFuture;

pin_project_lite::pin_project! {
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

impl<S, K, B> Service<Request<B>> for RateLimitService<S, K, B>
where
    S: Service<Request<B>, Future: TryFuture<Ok = S::Response, Error = S::Error>>,
    K: GetKey<B>,
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

    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        let now = std::time::Instant::now();

        let path = match req.extensions().get::<AxumMatchedPath>() {
            Some(path) => MatchedPath::Axum(path.clone()),
            None => MatchedPath::Root,
        };

        let builder = &self.layer.builder;

        let key = RouteWithKey {
            path,
            method: req.method().clone(),
            key: builder.get_key.get_key(&req),
        };

        let res = self.req_sync_peek_key(key, now, |key| {
            if let Some(ref set_ext) = builder.set_ext {
                // set_extension will clone the key internally
                set_ext.set_extension(req.extensions_mut(), key, self.layer.limiter.clone());
            }
        });

        match res {
            Err(e) => RateLimitedResponse::Err { e },
            Ok(()) => RateLimitedResponse::Ok {
                f: self.inner.call(req),
            },
        }
    }
}

impl<K, B, S> Layer<S> for RateLimitLayer<K, B>
where
    K: GetKey<B>,
{
    type Service = RateLimitService<S, K, B>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            layer: self.clone(),
        }
    }
}

use tower::layer::util::Stack;

impl<K, B> RateLimitLayerBuilder<K, B>
where
    K: GetKey<B>,
{
    /// Create a new rate limiter layer with the provided error-handler callback.
    pub fn handle_error<F, R>(
        self,
        cb: F,
    ) -> Stack<RateLimitLayer<K, B>, HandleErrorLayer<impl Fn(Error<Infallible>) -> R + Clone, ()>>
    where
        F: Fn(RateLimitError) -> R + Clone,
    {
        let limiter = Arc::new(gcra::RateLimiter::new(
            self.gc_interval.to_requests(),
            Default::default(),
        ));

        #[cfg(feature = "timed_gc")]
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

        Stack::new(
            RateLimitLayer {
                limiter,
                builder: Arc::new(self),
            },
            HandleErrorLayer::new(move |e| match e {
                Error::RateLimit(e) => cb(e),
                Error::Inner(_) => unreachable!(),
            }),
        )
    }

    /// Create a new rate limiter layer with the default error-handler callback that simply returns the error.
    pub fn default_handle_error(
        self,
    ) -> Stack<
        RateLimitLayer<K, B>,
        HandleErrorLayer<impl Fn(Error<Infallible>) -> Ready<RateLimitError> + Clone, ()>,
    > {
        self.handle_error(std::future::ready)
    }
}

/// [`Request`] extension to access the internal rate limiter used during that request,
/// such as to apply a penalty or reset the rate limit.
#[derive(Clone)]
pub struct RateLimiter<T: Hash + Eq> {
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
