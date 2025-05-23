//! Extended `JsonRpcClient` abstraction.
//!
//! This module contains custom implementation of `ethers::providers::JsonRpcClient`
//! which allows usage of non-`reqwest` based HTTP clients.
//!
//! The major type implemented in this module is the [JsonRpcProviderClient]
//! which implements the [ethers::providers::JsonRpcClient] trait. That makes it possible to use it with `ethers`.
//!
//! The [JsonRpcProviderClient] is abstract over the [HttpRequestor] trait, which makes it possible
//! to make the underlying HTTP client implementation easily replaceable. This is needed to make it possible
//! for `ethers` to work with different async runtimes, since the HTTP client is typically not agnostic to
//! async runtimes (the default HTTP client in `ethers` is using `reqwest`, which is `tokio` specific).
//! Secondly, this abstraction also allows implementing WASM-compatible HTTP client if needed at some point.

use async_trait::async_trait;
use ethers::providers::{JsonRpcClient, JsonRpcError};
use futures::StreamExt;
use http_types::Method;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Formatter};
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, trace, warn};
use validator::Validate;

use hopr_async_runtime::prelude::sleep;

use crate::client::RetryAction::{NoRetry, RetryAfter};
use crate::errors::{HttpRequestError, JsonRpcProviderClientError};
use crate::helper::{Request, Response};
use crate::{HttpRequestor, RetryAction, RetryPolicy};

#[cfg(all(feature = "prometheus", not(test)))]
use hopr_metrics::metrics::{MultiCounter, MultiHistogram};

#[cfg(all(feature = "prometheus", not(test)))]
lazy_static::lazy_static! {
    static ref METRIC_COUNT_RPC_CALLS: MultiCounter = MultiCounter::new(
        "hopr_rpc_call_count",
        "Number of Ethereum RPC calls over HTTP and their result",
        &["call", "result"]
    )
    .unwrap();
    static ref METRIC_RPC_CALLS_TIMING: MultiHistogram = MultiHistogram::new(
        "hopr_rpc_call_time_sec",
        "Timing of RPC calls over HTTP in seconds",
        vec![0.1, 0.5, 1.0, 2.0, 5.0, 7.0, 10.0],
        &["call"]
    )
    .unwrap();
    static ref METRIC_RETRIES_PER_RPC_CALL: MultiHistogram = MultiHistogram::new(
        "hopr_retries_per_rpc_call",
        "Number of retries per RPC call",
        vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0],
        &["call"]
    )
    .unwrap();
}

/// Defines a retry policy suitable for `JsonRpcProviderClient`.
///
/// This retry policy distinguishes between 4 types of RPC request failures:
/// - JSON RPC error (based on error code)
/// - HTTP error (based on HTTP status)
/// - Transport error (e.g. connection timeout)
/// - Serde error (some of these are treated as JSON RPC error above, if an error code can be obtained).
///
/// The policy will make up to `max_retries` once a JSON RPC request fails.
/// The minimum number of retries `min_retries` can be also specified and applies to any type of error regardless.
/// Each retry `k > 0` will be separated by a delay of `initial_backoff * (1 + backoff_coefficient)^(k - 1)`,
/// namely all the JSON RPC error codes specified in `retryable_json_rpc_errors` and all the HTTP errors
/// specified in `retryable_http_errors`.
///
/// The total wait time will be `(initial_backoff/backoff_coefficient) * ((1 + backoff_coefficient)^max_retries - 1)`.
/// or `max_backoff`, whatever is lower.
///
/// Transport and connection errors (such as connection timeouts) are retried without backoff
/// at a constant delay of `initial_backoff` if `backoff_on_transport_errors` is not set.
///
/// No more additional retries are allowed on new requests, if the maximum number of concurrent
/// requests being retried has reached `max_retry_queue_size`.
#[derive(Clone, Debug, PartialEq, smart_default::SmartDefault, Serialize, Deserialize, Validate)]
pub struct SimpleJsonRpcRetryPolicy {
    /// Minimum number of retries of any error, regardless the error code.
    ///
    /// Default is 0.
    #[validate(range(min = 0))]
    #[default(Some(0))]
    pub min_retries: Option<u32>,

    /// Maximum number of retries.
    ///
    /// If `None` is given, will keep retrying indefinitely.
    ///
    /// Default is 12.
    #[validate(range(min = 1))]
    #[default(Some(12))]
    pub max_retries: Option<u32>,
    /// Initial wait before retries.
    ///
    /// NOTE: Transport and connection errors (such as connection timeouts) are retried at
    /// a constant rate (no backoff) with this delay if `backoff_on_transport_errors` is not set.
    ///
    /// Default is 1 second.
    #[default(Duration::from_secs(1))]
    pub initial_backoff: Duration,
    /// Backoff coefficient by which will be each retry multiplied.
    ///
    /// Must be non-negative. If set to `0`, no backoff will be applied and the
    /// requests will be retried at a constant rate.
    ///
    /// Default is 0.3
    #[validate(range(min = 0.0))]
    #[default(0.3)]
    pub backoff_coefficient: f64,
    /// Maximum backoff value.
    ///
    /// Once reached, the requests will be retried at a constant rate with this timeout.
    ///
    /// Default is 30 seconds.
    #[default(Duration::from_secs(30))]
    pub max_backoff: Duration,
    /// Indicates whether to also apply backoff to transport and connection errors (such as connection timeouts).
    ///
    /// Default is false.
    pub backoff_on_transport_errors: bool,
    /// List of JSON RPC errors that should be retried with backoff
    ///
    /// Default is \[429, -32005, -32016\]
    #[default(_code = "vec![-32005, -32016, 429]")]
    pub retryable_json_rpc_errors: Vec<i64>,

    /// List of HTTP errors that should be retried with backoff.
    ///
    /// Default is \[429, 504, 503\]
    #[default(
        _code = "vec![http_types::StatusCode::TooManyRequests,http_types::StatusCode::GatewayTimeout,http_types::StatusCode::ServiceUnavailable]"
    )]
    pub retryable_http_errors: Vec<http_types::StatusCode>,
    /// Maximum number of different requests that are being retried at the same time.
    ///
    /// If any additional request fails after this number is attained, it won't be retried.
    ///
    /// Default is 100
    #[validate(range(min = 5))]
    #[default = 100]
    pub max_retry_queue_size: u32,
}

impl SimpleJsonRpcRetryPolicy {
    fn is_retryable_json_rpc_error(&self, err: &JsonRpcError) -> bool {
        self.retryable_json_rpc_errors.contains(&err.code) || err.message.contains("rate limit")
    }

    fn is_retryable_http_error(&self, status: &http_types::StatusCode) -> bool {
        self.retryable_http_errors.contains(status)
    }
}

impl RetryPolicy<JsonRpcProviderClientError> for SimpleJsonRpcRetryPolicy {
    fn is_retryable_error(
        &self,
        err: &JsonRpcProviderClientError,
        num_retries: u32,
        retry_queue_size: u32,
    ) -> RetryAction {
        if self.max_retries.is_some_and(|max| num_retries > max) {
            warn!(
                count = self.max_retries.expect("max_retries must be set"),
                "max number of retries has been reached"
            );
            return NoRetry;
        }

        debug!(
            size = retry_queue_size,
            "checking retry queue size after retryable error"
        );

        if retry_queue_size > self.max_retry_queue_size {
            warn!(
                size = self.max_retry_queue_size,
                "maximum size of retry queue has been reached"
            );
            return NoRetry;
        }

        // next_backoff = initial_backoff * (1 + backoff_coefficient)^(num_retries - 1)
        let backoff = self
            .initial_backoff
            .mul_f64(f64::powi(1.0 + self.backoff_coefficient, (num_retries - 1) as i32))
            .min(self.max_backoff);

        // Retry if a global minimum of number of retries was given and wasn't yet attained
        if self.min_retries.is_some_and(|min| num_retries <= min) {
            debug!(num_retries, min_retries = ?self.min_retries,  "retrying because minimum number of retries not yet reached");
            return RetryAfter(backoff);
        }

        match err {
            // Retryable JSON RPC errors are retries with backoff
            JsonRpcProviderClientError::JsonRpcError(e) if self.is_retryable_json_rpc_error(e) => {
                debug!(error = %e, "encountered retryable JSON RPC error code");
                RetryAfter(backoff)
            }

            // Retryable HTTP errors are retries with backoff
            JsonRpcProviderClientError::BackendError(HttpRequestError::HttpError(e))
                if self.is_retryable_http_error(e) =>
            {
                debug!(error = ?e, "encountered retryable HTTP error code");
                RetryAfter(backoff)
            }

            // Transport error and timeouts are retried at a constant rate if specified
            JsonRpcProviderClientError::BackendError(e @ HttpRequestError::Timeout)
            | JsonRpcProviderClientError::BackendError(e @ HttpRequestError::TransportError(_))
            | JsonRpcProviderClientError::BackendError(e @ HttpRequestError::UnknownError(_)) => {
                debug!(error = %e, "encountered retryable transport error");
                RetryAfter(if self.backoff_on_transport_errors {
                    backoff
                } else {
                    self.initial_backoff
                })
            }

            // Some providers send invalid JSON RPC in the error case (no `id:u64`), but the text is a `JsonRpcError`
            JsonRpcProviderClientError::SerdeJson { text, .. } => {
                #[derive(Deserialize)]
                struct Resp {
                    error: JsonRpcError,
                }

                match serde_json::from_str::<Resp>(text) {
                    Ok(Resp { error }) if self.is_retryable_json_rpc_error(&error) => {
                        debug!(%error, "encountered retryable JSON RPC error");
                        RetryAfter(backoff)
                    }
                    _ => {
                        debug!(error = %text, "unparseable JSON RPC error");
                        NoRetry
                    }
                }
            }

            // Anything else is not retried
            _ => NoRetry,
        }
    }
}

/// Modified implementation of `ethers::providers::Http` so that it can
/// operate with any `HttpPostRequestor`.
/// Also contains possible retry actions to be taken on various failures, therefore it
/// implements also `ethers::providers::RetryClient` functionality.
pub struct JsonRpcProviderClient<Req: HttpRequestor, R: RetryPolicy<JsonRpcProviderClientError>> {
    id: AtomicU64,
    requests_enqueued: AtomicU32,
    url: String,
    requestor: Req,
    retry_policy: R,
}

impl<Req: HttpRequestor, R: RetryPolicy<JsonRpcProviderClientError>> JsonRpcProviderClient<Req, R> {
    /// Creates the client given the `HttpPostRequestor`
    pub fn new(base_url: &str, requestor: Req, retry_policy: R) -> Self {
        Self {
            id: AtomicU64::new(1),
            requests_enqueued: AtomicU32::new(0),
            url: base_url.to_owned(),
            requestor,
            retry_policy,
        }
    }

    async fn send_request_internal<T, A>(&self, method: &str, params: T) -> Result<A, JsonRpcProviderClientError>
    where
        T: Serialize + Send + Sync,
        A: DeserializeOwned,
    {
        // Create the Request object
        let next_id = self.id.fetch_add(1, Ordering::SeqCst);
        let payload = Request::new(next_id, method, params);

        debug!(method, "sending rpc request");
        trace!(
            method,
            request = serde_json::to_string(&payload).expect("request must be serializable"),
            "sending rpc request",
        );

        // Perform the actual request
        let start = std::time::Instant::now();
        let body = self.requestor.http_post(self.url.as_ref(), payload).await?;
        let req_duration = start.elapsed();

        trace!(method, duration_in_ms = req_duration.as_millis(), "rpc request took");

        #[cfg(all(feature = "prometheus", not(test)))]
        METRIC_RPC_CALLS_TIMING.observe(&[method], req_duration.as_secs_f64());

        // First deserialize the Response object
        let raw = match serde_json::from_slice(&body) {
            Ok(Response::Success { result, .. }) => result.to_owned(),
            Ok(Response::Error { error, .. }) => {
                #[cfg(all(feature = "prometheus", not(test)))]
                METRIC_COUNT_RPC_CALLS.increment(&[method, "failure"]);

                return Err(error.into());
            }
            Ok(_) => {
                let err = JsonRpcProviderClientError::SerdeJson {
                    err: serde::de::Error::custom("unexpected notification over HTTP transport"),
                    text: String::from_utf8_lossy(&body).to_string(),
                };
                #[cfg(all(feature = "prometheus", not(test)))]
                METRIC_COUNT_RPC_CALLS.increment(&[method, "failure"]);

                return Err(err);
            }
            Err(err) => {
                #[cfg(all(feature = "prometheus", not(test)))]
                METRIC_COUNT_RPC_CALLS.increment(&[method, "failure"]);

                return Err(JsonRpcProviderClientError::SerdeJson {
                    err,
                    text: String::from_utf8_lossy(&body).to_string(),
                });
            }
        };

        // Next, deserialize the data out of the Response object
        let json_str = raw.get();
        trace!(method, response = &json_str, "rpc request response received");

        let res = serde_json::from_str(json_str).map_err(|err| JsonRpcProviderClientError::SerdeJson {
            err,
            text: raw.to_string(),
        })?;

        #[cfg(all(feature = "prometheus", not(test)))]
        METRIC_COUNT_RPC_CALLS.increment(&[method, "success"]);

        Ok(res)
    }
}

impl<Req: HttpRequestor, R: RetryPolicy<JsonRpcProviderClientError>> Debug for JsonRpcProviderClient<Req, R> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonRpcProviderClient")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("requests_enqueued", &self.requests_enqueued)
            .finish_non_exhaustive()
    }
}

impl<Req: HttpRequestor + Clone, R: RetryPolicy<JsonRpcProviderClientError> + Clone> Clone
    for JsonRpcProviderClient<Req, R>
{
    fn clone(&self) -> Self {
        Self {
            id: AtomicU64::new(1),
            url: self.url.clone(),
            requests_enqueued: AtomicU32::new(0),
            requestor: self.requestor.clone(),
            retry_policy: self.retry_policy.clone(),
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl<Req, R> JsonRpcClient for JsonRpcProviderClient<Req, R>
where
    Req: HttpRequestor,
    R: RetryPolicy<JsonRpcProviderClientError> + Send + Sync,
{
    type Error = JsonRpcProviderClientError;

    async fn request<T, A>(&self, method: &str, params: T) -> Result<A, Self::Error>
    where
        T: Serialize + Send + Sync,
        A: DeserializeOwned + Send,
    {
        // Helper type that caches the `params` value across several retries
        // This is necessary because the wrapper provider is supposed to skip he `params` if it's of
        // size 0, see `crate::transports::common::Request`
        enum RetryParams<Params> {
            Value(Params),
            Zst(()),
        }

        let params = if std::mem::size_of::<A>() == 0 {
            RetryParams::Zst(())
        } else {
            let params = serde_json::to_value(params)
                .map_err(|err| JsonRpcProviderClientError::SerdeJson { err, text: "".into() })?;
            RetryParams::Value(params)
        };

        self.requests_enqueued.fetch_add(1, Ordering::SeqCst);
        let start = std::time::Instant::now();

        let mut num_retries = 0;
        loop {
            let err;

            // hack to not hold `A` across an await in the sleep future and prevent requiring
            // A: Send + Sync
            {
                let resp = match params {
                    RetryParams::Value(ref params) => self.send_request_internal(method, params).await,
                    RetryParams::Zst(unit) => self.send_request_internal(method, unit).await,
                };

                match resp {
                    Ok(ret) => {
                        self.requests_enqueued.fetch_sub(1, Ordering::SeqCst);

                        #[cfg(all(feature = "prometheus", not(test)))]
                        METRIC_RETRIES_PER_RPC_CALL.observe(&[method], num_retries as f64);

                        debug!(method, elapsed_in_ms = start.elapsed().as_millis(), "request succeeded",);
                        return Ok(ret);
                    }
                    Err(req_err) => {
                        err = req_err;
                        error!(
                            method,
                            elapsed_in_ms = start.elapsed().as_millis(),
                            error = %err,
                            "request failed",
                        );
                        num_retries += 1;
                    }
                }
            }

            match self
                .retry_policy
                .is_retryable_error(&err, num_retries, self.requests_enqueued.load(Ordering::SeqCst))
            {
                NoRetry => {
                    self.requests_enqueued.fetch_sub(1, Ordering::SeqCst);
                    warn!(method, "no more retries for RPC call");

                    #[cfg(all(feature = "prometheus", not(test)))]
                    METRIC_RETRIES_PER_RPC_CALL.observe(&[method], num_retries as f64);

                    debug!(
                        method,
                        duration_in_ms = start.elapsed().as_millis(),
                        "failed request duration in the retry queue",
                    );
                    return Err(err);
                }
                RetryAfter(backoff) => {
                    warn!(method, backoff_in_ms = backoff.as_millis(), "request will retry",);
                    sleep(backoff).await
                }
            }
        }
    }
}

#[cfg(any(test, feature = "runtime-async-std"))]
pub mod surf_client {
    use async_std::prelude::FutureExt;
    use async_trait::async_trait;
    use serde::Serialize;
    use tracing::info;

    use crate::errors::HttpRequestError;
    use crate::{HttpPostRequestorConfig, HttpRequestor};

    /// HTTP client that uses a non-Tokio runtime based HTTP client library, such as `surf`.
    /// `surf` works also for Browsers in WASM environments.
    #[derive(Clone, Debug, Default)]
    pub struct SurfRequestor {
        client: surf::Client,
        cfg: HttpPostRequestorConfig,
    }

    impl SurfRequestor {
        pub fn new(cfg: HttpPostRequestorConfig) -> Self {
            info!(?cfg, "creating surf client");

            let mut client = surf::client().with(surf::middleware::Redirect::new(cfg.max_redirects));

            // Rate limit of 0 also means unlimited as if None was given
            if let Some(max) = cfg.max_requests_per_sec.and_then(|r| (r > 0).then_some(r)) {
                client = client.with(
                    surf_governor::GovernorMiddleware::per_second(max)
                        .expect("cannot setup http rate limiter middleware"),
                );
            }

            Self { client, cfg }
        }
    }

    #[async_trait]
    impl HttpRequestor for SurfRequestor {
        async fn http_query<T>(
            &self,
            method: http_types::Method,
            url: &str,
            data: Option<T>,
        ) -> Result<Box<[u8]>, HttpRequestError>
        where
            T: Serialize + Send + Sync,
        {
            let request = match method {
                http_types::Method::Post => self
                    .client
                    .post(url)
                    .body_json(&data.ok_or(HttpRequestError::UnknownError("missing data".to_string()))?)
                    .map_err(|e| HttpRequestError::UnknownError(e.to_string()))?,
                http_types::Method::Get => self.client.get(url),
                _ => return Err(HttpRequestError::UnknownError("unsupported method".to_string())),
            };

            async move {
                match request.await {
                    Ok(mut response) if response.status().is_success() => match response.body_bytes().await {
                        Ok(data) => Ok(data.into_boxed_slice()),
                        Err(e) => Err(HttpRequestError::TransportError(e.to_string())),
                    },
                    Ok(response) => Err(HttpRequestError::HttpError(response.status())),
                    Err(e) => Err(HttpRequestError::TransportError(e.to_string())),
                }
            }
            .timeout(self.cfg.http_request_timeout)
            .await
            .map_err(|_| HttpRequestError::Timeout)?
        }
    }
}

#[cfg(any(test, feature = "runtime-tokio"))]
pub mod reqwest_client {
    use async_trait::async_trait;
    use http_types::StatusCode;
    use serde::Serialize;
    use std::sync::Arc;
    use std::time::Duration;
    use tracing::info;

    use crate::errors::HttpRequestError;
    use crate::{HttpPostRequestorConfig, HttpRequestor};

    /// HTTP client that uses a Tokio runtime-based HTTP client library, such as `reqwest`.
    #[derive(Clone, Debug, Default)]
    pub struct ReqwestRequestor {
        client: reqwest::Client,
        limiter: Option<Arc<governor::DefaultKeyedRateLimiter<String>>>,
    }

    impl ReqwestRequestor {
        pub fn new(cfg: HttpPostRequestorConfig) -> Self {
            info!(?cfg, "creating reqwest client");
            Self {
                client: reqwest::Client::builder()
                    .timeout(cfg.http_request_timeout)
                    .redirect(reqwest::redirect::Policy::limited(cfg.max_redirects as usize))
                    // 30 seconds is longer than the normal interval between RPC requests, thus the
                    // connection should remain available
                    .tcp_keepalive(Some(Duration::from_secs(30)))
                    // Enable all supported encodings to reduce the amount of data transferred
                    // in responses. This is relevant for large eth_getLogs responses.
                    .zstd(true)
                    .brotli(true)
                    .build()
                    .expect("could not build reqwest client"),
                limiter: cfg
                    .max_requests_per_sec
                    .filter(|reqs| *reqs > 0) // Ensures the following unwrapping won't fail
                    .map(|reqs| {
                        Arc::new(governor::DefaultKeyedRateLimiter::keyed(governor::Quota::per_second(
                            reqs.try_into().unwrap(),
                        )))
                    }),
            }
        }
    }

    #[async_trait]
    impl HttpRequestor for ReqwestRequestor {
        async fn http_query<T>(
            &self,
            method: http_types::Method,
            url: &str,
            data: Option<T>,
        ) -> Result<Box<[u8]>, HttpRequestError>
        where
            T: Serialize + Send + Sync,
        {
            let url = reqwest::Url::parse(url)
                .map_err(|e| HttpRequestError::UnknownError(format!("url parse error: {e}")))?;

            let builder = match method {
                http_types::Method::Get => self.client.get(url.clone()),
                http_types::Method::Post => self.client.post(url.clone()).body(
                    serde_json::to_string(&data.ok_or(HttpRequestError::UnknownError("missing data".to_string()))?)
                        .map_err(|e| HttpRequestError::UnknownError(format!("serialize error: {e}")))?,
                ),
                _ => return Err(HttpRequestError::UnknownError("unsupported method".to_string())),
            };

            if self
                .limiter
                .clone()
                .map(|limiter| limiter.check_key(&url.host_str().unwrap_or(".").to_string()).is_ok())
                .unwrap_or(true)
            {
                let resp = builder
                    .header("content-type", "application/json")
                    .send()
                    .await
                    .map_err(|e| {
                        if e.is_status() {
                            HttpRequestError::HttpError(
                                StatusCode::try_from(e.status().map(|s| s.as_u16()).unwrap_or(500))
                                    .expect("status code must be compatible"), // cannot happen
                            )
                        } else if e.is_timeout() {
                            HttpRequestError::Timeout
                        } else {
                            HttpRequestError::UnknownError(e.to_string())
                        }
                    })?;

                resp.bytes()
                    .await
                    .map(|b| Box::from(b.as_ref()))
                    .map_err(|e| HttpRequestError::UnknownError(format!("error retrieving body: {e}")))
            } else {
                Err(HttpRequestError::HttpError(StatusCode::TooManyRequests))
            }
        }
    }
}

/// Snapshot of a response cached by the [`SnapshotRequestor`].
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct RequestorResponseSnapshot {
    id: usize,
    request: String,
    response: String,
}

/// Replays an RPC response to a request if it is found in the snapshot YAML file.
/// If no such request has been seen before,
/// it captures the new request/response pair obtained from the inner [`HttpRequestor`]
/// and stores it into the snapshot file.
///
/// This is useful for snapshot testing only and should **NOT** be used in production.
#[derive(Debug, Clone)]
pub struct SnapshotRequestor<T> {
    inner: T,
    next_id: Arc<AtomicUsize>,
    entries: moka::future::Cache<String, RequestorResponseSnapshot>,
    file: String,
    aggressive_save: bool,
    fail_on_miss: bool,
    ignore_snapshot: bool,
}

impl<T> SnapshotRequestor<T> {
    /// Creates a new instance by wrapping an existing [`HttpRequestor`] and capturing
    /// the request/response pairs.
    ///
    /// The constructor does not load any [snapshot entries](SnapshotRequestor) from
    /// the `snapshot_file`.
    /// The [`SnapshotRequestor::load`] method must be used after construction to do that.
    pub fn new(inner: T, snapshot_file: &str) -> Self {
        Self {
            inner,
            next_id: Arc::new(AtomicUsize::new(1)),
            entries: moka::future::Cache::builder().build(),
            file: snapshot_file.to_owned(),
            aggressive_save: false,
            fail_on_miss: false,
            ignore_snapshot: false,
        }
    }

    /// Gets the path to the snapshot disk file.
    pub fn snapshot_path(&self) -> &str {
        &self.file
    }

    /// Clears all entries from the snapshot in memory.
    /// The snapshot file is not changed.
    pub fn clear(&self) {
        self.entries.invalidate_all();
        self.next_id.store(1, Ordering::Relaxed);
    }

    /// Clears all entries and loads them from the snapshot file.
    /// If `fail_on_miss` is set and the data is successfully loaded, all later
    /// requests that miss the loaded snapshot will result in HTTP error 404.
    pub async fn try_load(&mut self, fail_on_miss: bool) -> Result<(), std::io::Error> {
        if self.ignore_snapshot {
            return Ok(());
        }

        let loaded = serde_yaml::from_reader::<_, Vec<RequestorResponseSnapshot>>(std::fs::File::open(&self.file)?)
            .map_err(std::io::Error::other)?;

        self.clear();

        let loaded_len = futures::stream::iter(loaded)
            .then(|entry| {
                self.next_id.fetch_max(entry.id, Ordering::Relaxed);
                self.entries.insert(entry.request.clone(), entry)
            })
            .collect::<Vec<_>>()
            .await
            .len();

        if loaded_len > 0 {
            self.fail_on_miss = fail_on_miss;
        }

        tracing::debug!("snapshot with {loaded_len} entries has been loaded from {}", &self.file);
        Ok(())
    }

    /// Similar as [`SnapshotRequestor::try_load`], except that no entries are cleared if the load fails.
    ///
    /// This method consumes and returns self for easier call chaining.
    pub async fn load(mut self, fail_on_miss: bool) -> Self {
        let _ = self.try_load(fail_on_miss).await;
        self
    }

    /// Forces saving to disk on each newly inserted entry.
    ///
    /// Use this only when the expected number of entries in the snapshot is small.
    pub fn with_aggresive_save(mut self) -> Self {
        self.aggressive_save = true;
        self
    }

    /// If set, the snapshot data will be ignored and resolution
    /// will always be done with the inner requestor.
    ///
    /// This will inhibit any attempts to [`load`](SnapshotRequestor::try_load) or
    /// [`save`](SnapshotRequestor::save) snapshot data.
    pub fn with_ignore_snapshot(mut self, ignore_snapshot: bool) -> Self {
        self.ignore_snapshot = ignore_snapshot;
        self
    }

    /// Save the currently cached entries to the snapshot file on disk.
    ///
    /// Note that this method is automatically called on Drop, so usually it is unnecessary
    /// to call it explicitly.
    pub fn save(&self) -> Result<(), std::io::Error> {
        if self.ignore_snapshot {
            return Ok(());
        }

        let mut values: Vec<RequestorResponseSnapshot> = self.entries.iter().map(|(_, r)| r).collect();
        values.sort_unstable_by_key(|a| a.id);

        let mut writer = BufWriter::new(std::fs::File::create(&self.file)?);

        serde_yaml::to_writer(&mut writer, &values).map_err(std::io::Error::other)?;

        writer.flush()?;

        tracing::debug!("snapshot with {} entries saved to file {}", values.len(), self.file);
        Ok(())
    }
}

impl<R: HttpRequestor> SnapshotRequestor<R> {
    async fn http_post_with_snapshot<In>(&self, url: &str, data: In) -> Result<Box<[u8]>, HttpRequestError>
    where
        In: Serialize + Send + Sync,
    {
        let request = serde_json::to_string(&data)
            .map_err(|e| HttpRequestError::UnknownError(format!("serialize error: {e}")))?;

        let inserted = AtomicBool::new(false);
        let result = self
            .entries
            .entry(request.clone())
            .or_try_insert_with(async {
                if self.fail_on_miss {
                    tracing::error!("{request} is missing in {}", &self.file);
                    return Err(HttpRequestError::HttpError(http_types::StatusCode::NotFound));
                }

                let response = self.inner.http_post(url, data).await?;
                let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                inserted.store(true, Ordering::Relaxed);

                tracing::debug!("saved new snapshot entry #{id}");
                Ok(RequestorResponseSnapshot {
                    id,
                    request: request.clone(),
                    response: String::from_utf8(response.into_vec())
                        .map_err(|e| HttpRequestError::UnknownError(format!("unparseable data: {e}")))?,
                })
            })
            .await
            .map(|e| e.into_value().response.into_bytes().into_boxed_slice())
            .map_err(|e: Arc<HttpRequestError>| e.as_ref().clone())?;

        if inserted.load(Ordering::Relaxed) && self.aggressive_save {
            tracing::debug!("{request} was NOT found and was resolved");
            self.save().map_err(|e| HttpRequestError::UnknownError(e.to_string()))?;
        } else {
            tracing::debug!("{request} was found");
        }

        Ok(result)
    }
}

impl<T> Drop for SnapshotRequestor<T> {
    fn drop(&mut self) {
        if let Err(e) = self.save() {
            tracing::error!("failed to save snapshot: {e}");
        }
    }
}

#[async_trait::async_trait]
impl<R: HttpRequestor> HttpRequestor for SnapshotRequestor<R> {
    async fn http_query<T>(&self, _: Method, _: &str, _: Option<T>) -> Result<Box<[u8]>, HttpRequestError>
    where
        T: Serialize + Send + Sync,
    {
        todo!()
    }

    async fn http_post<T>(&self, url: &str, data: T) -> Result<Box<[u8]>, HttpRequestError>
    where
        T: Serialize + Send + Sync,
    {
        self.http_post_with_snapshot(url, data).await
    }

    async fn http_get(&self, _url: &str) -> Result<Box<[u8]>, HttpRequestError> {
        todo!()
    }
}

#[async_trait]
impl<R: HttpRequestor> HttpRequestor for &SnapshotRequestor<R> {
    async fn http_query<T>(&self, _: Method, _: &str, _: Option<T>) -> Result<Box<[u8]>, HttpRequestError>
    where
        T: Serialize + Send + Sync,
    {
        todo!()
    }

    async fn http_post<T>(&self, url: &str, data: T) -> Result<Box<[u8]>, HttpRequestError>
    where
        T: Serialize + Send + Sync,
    {
        self.http_post_with_snapshot(url, data).await
    }

    async fn http_get(&self, _url: &str) -> Result<Box<[u8]>, HttpRequestError> {
        todo!()
    }
}

type AnvilRpcClient<R> = ethers::middleware::SignerMiddleware<
    ethers::providers::Provider<JsonRpcProviderClient<R, SimpleJsonRpcRetryPolicy>>,
    ethers::signers::Wallet<ethers::core::k256::ecdsa::SigningKey>,
>;

/// Used for testing. Creates Ethers RPC client to the local Anvil instance.
#[cfg(not(target_arch = "wasm32"))]
pub fn create_rpc_client_to_anvil<R: HttpRequestor>(
    backend: R,
    anvil: &ethers::utils::AnvilInstance,
    signer: &hopr_crypto_types::keypairs::ChainKeypair,
) -> Arc<AnvilRpcClient<R>> {
    use ethers::signers::Signer;
    use hopr_crypto_types::keypairs::Keypair;

    let wallet =
        ethers::signers::LocalWallet::from_bytes(signer.secret().as_ref()).expect("failed to construct wallet");
    let json_client = JsonRpcProviderClient::new(&anvil.endpoint(), backend, SimpleJsonRpcRetryPolicy::default());
    let provider = ethers::providers::Provider::new(json_client).interval(Duration::from_millis(10_u64));

    Arc::new(ethers::middleware::SignerMiddleware::new(
        provider,
        wallet.with_chain_id(anvil.chain_id()),
    ))
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use ethers::providers::JsonRpcClient;
    use hopr_async_runtime::prelude::sleep;
    use hopr_chain_types::utils::create_anvil;
    use hopr_chain_types::{ContractAddresses, ContractInstances};
    use hopr_crypto_types::keypairs::{ChainKeypair, Keypair};
    use hopr_primitive_types::primitives::Address;
    use http_types::Method;
    use serde::Serialize;
    use serde_json::json;
    use std::fmt::Debug;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tempfile::NamedTempFile;

    use crate::client::reqwest_client::ReqwestRequestor;
    use crate::client::surf_client::SurfRequestor;
    use crate::client::{
        create_rpc_client_to_anvil, JsonRpcProviderClient, SimpleJsonRpcRetryPolicy, SnapshotRequestor,
    };
    use crate::errors::{HttpRequestError, JsonRpcProviderClientError};
    use crate::{HttpRequestor, ZeroRetryPolicy};

    async fn deploy_contracts<R: HttpRequestor + Debug>(req: R) -> anyhow::Result<ContractAddresses> {
        let anvil = create_anvil(None);
        let chain_key_0 = ChainKeypair::from_secret(anvil.keys()[0].to_bytes().as_ref())?;

        let client = create_rpc_client_to_anvil(req, &anvil, &chain_key_0);

        let contracts = ContractInstances::deploy_for_testing(client.clone(), &chain_key_0)
            .await
            .expect("deploy failed");

        Ok(ContractAddresses::from(&contracts))
    }

    #[async_std::test]
    async fn test_client_should_deploy_contracts_via_surf() -> anyhow::Result<()> {
        let contract_addrs = deploy_contracts(SurfRequestor::default()).await?;

        assert_ne!(contract_addrs.token, Address::default());
        assert_ne!(contract_addrs.channels, Address::default());
        assert_ne!(contract_addrs.announcements, Address::default());
        assert_ne!(contract_addrs.network_registry, Address::default());
        assert_ne!(contract_addrs.safe_registry, Address::default());
        assert_ne!(contract_addrs.price_oracle, Address::default());

        Ok(())
    }

    #[tokio::test]
    async fn test_client_should_deploy_contracts_via_reqwest() -> anyhow::Result<()> {
        let contract_addrs = deploy_contracts(ReqwestRequestor::default()).await?;

        assert_ne!(contract_addrs.token, Address::default());
        assert_ne!(contract_addrs.channels, Address::default());
        assert_ne!(contract_addrs.announcements, Address::default());
        assert_ne!(contract_addrs.network_registry, Address::default());
        assert_ne!(contract_addrs.safe_registry, Address::default());
        assert_ne!(contract_addrs.price_oracle, Address::default());

        Ok(())
    }

    #[async_std::test]
    async fn test_client_should_get_block_number() -> anyhow::Result<()> {
        let block_time = Duration::from_secs(1);

        let anvil = create_anvil(Some(block_time));
        let client = JsonRpcProviderClient::new(
            &anvil.endpoint(),
            SurfRequestor::default(),
            SimpleJsonRpcRetryPolicy::default(),
        );

        let mut last_number = 0;

        for _ in 0..3 {
            sleep(block_time).await;

            let number: ethers::types::U64 = client.request("eth_blockNumber", ()).await?;

            assert!(number.as_u64() > last_number, "next block number must be greater");
            last_number = number.as_u64();
        }

        assert_eq!(
            0,
            client.requests_enqueued.load(Ordering::SeqCst),
            "retry queue should be zero on successful requests"
        );

        Ok(())
    }

    #[async_std::test]
    async fn test_client_should_fail_on_malformed_request() {
        let anvil = create_anvil(None);
        let client = JsonRpcProviderClient::new(
            &anvil.endpoint(),
            SurfRequestor::default(),
            SimpleJsonRpcRetryPolicy::default(),
        );

        let err = client
            .request::<_, ethers::types::U64>("eth_blockNumber_bla", ())
            .await
            .expect_err("expected error");

        assert!(matches!(err, JsonRpcProviderClientError::JsonRpcError(..)));
    }

    #[async_std::test]
    async fn test_client_should_fail_on_malformed_response() {
        let mut server = mockito::Server::new_async().await;

        let m = server
            .mock("POST", "/")
            .with_status(200)
            .match_body(mockito::Matcher::PartialJson(json!({"method": "eth_blockNumber"})))
            .with_body("}malformed{")
            .expect(1)
            .create();

        let client = JsonRpcProviderClient::new(
            &server.url(),
            SurfRequestor::default(),
            SimpleJsonRpcRetryPolicy::default(),
        );

        let err = client
            .request::<_, ethers::types::U64>("eth_blockNumber", ())
            .await
            .expect_err("expected error");

        m.assert();
        assert!(matches!(err, JsonRpcProviderClientError::SerdeJson { .. }));
    }

    #[async_std::test]
    async fn test_client_should_retry_on_http_error() {
        let mut server = mockito::Server::new_async().await;

        let m = server
            .mock("POST", "/")
            .with_status(http_types::StatusCode::TooManyRequests as usize)
            .match_body(mockito::Matcher::PartialJson(json!({"method": "eth_blockNumber"})))
            .with_body("{}")
            .expect(3)
            .create();

        let client = JsonRpcProviderClient::new(
            &server.url(),
            SurfRequestor::default(),
            SimpleJsonRpcRetryPolicy {
                max_retries: Some(2),
                retryable_http_errors: vec![http_types::StatusCode::TooManyRequests],
                initial_backoff: Duration::from_millis(100),
                ..SimpleJsonRpcRetryPolicy::default()
            },
        );

        let err = client
            .request::<_, ethers::types::U64>("eth_blockNumber", ())
            .await
            .expect_err("expected error");

        m.assert();
        assert!(matches!(err, JsonRpcProviderClientError::BackendError(_)));
        assert_eq!(
            0,
            client.requests_enqueued.load(Ordering::SeqCst),
            "retry queue should be zero when policy says no more retries"
        );
    }

    #[async_std::test]
    async fn test_client_should_not_retry_with_zero_retry_policy() {
        let mut server = mockito::Server::new_async().await;

        let m = server
            .mock("POST", "/")
            .with_status(404)
            .match_body(mockito::Matcher::PartialJson(json!({"method": "eth_blockNumber"})))
            .with_body("{}")
            .expect(1)
            .create();

        let client = JsonRpcProviderClient::new(&server.url(), SurfRequestor::default(), ZeroRetryPolicy::default());

        let err = client
            .request::<_, ethers::types::U64>("eth_blockNumber", ())
            .await
            .expect_err("expected error");

        m.assert();
        assert!(matches!(err, JsonRpcProviderClientError::BackendError(_)));
        assert_eq!(
            0,
            client.requests_enqueued.load(Ordering::SeqCst),
            "retry queue should be zero when policy says no more retries"
        );
    }

    #[async_std::test]
    async fn test_client_should_retry_on_json_rpc_error() {
        let mut server = mockito::Server::new_async().await;

        let m = server
            .mock("POST", "/")
            .with_status(200)
            .match_body(mockito::Matcher::PartialJson(json!({"method": "eth_blockNumber"})))
            .with_body(
                r#"{
              "jsonrpc": "2.0",
              "id": 1,
              "error": {
                "message": "some message",
                "code": -32603
              }
            }"#,
            )
            .expect(3)
            .create();

        let client = JsonRpcProviderClient::new(
            &server.url(),
            SurfRequestor::default(),
            SimpleJsonRpcRetryPolicy {
                max_retries: Some(2),
                retryable_json_rpc_errors: vec![-32603],
                initial_backoff: Duration::from_millis(100),
                ..SimpleJsonRpcRetryPolicy::default()
            },
        );

        let err = client
            .request::<_, ethers::types::U64>("eth_blockNumber", ())
            .await
            .expect_err("expected error");

        m.assert();
        assert!(matches!(err, JsonRpcProviderClientError::JsonRpcError(_)));
        assert_eq!(
            0,
            client.requests_enqueued.load(Ordering::SeqCst),
            "retry queue should be zero when policy says no more retries"
        );
    }

    #[async_std::test]
    async fn test_client_should_not_retry_on_nonretryable_json_rpc_error() {
        let mut server = mockito::Server::new_async().await;

        let m = server
            .mock("POST", "/")
            .with_status(200)
            .match_body(mockito::Matcher::PartialJson(json!({"method": "eth_blockNumber"})))
            .with_body(
                r#"{
              "jsonrpc": "2.0",
              "id": 1,
              "error": {
                "message": "some message",
                "code": -32000
              }
            }"#,
            )
            .expect(1)
            .create();

        let client = JsonRpcProviderClient::new(
            &server.url(),
            SurfRequestor::default(),
            SimpleJsonRpcRetryPolicy {
                max_retries: Some(2),
                retryable_json_rpc_errors: vec![],
                initial_backoff: Duration::from_millis(100),
                ..SimpleJsonRpcRetryPolicy::default()
            },
        );

        let err = client
            .request::<_, ethers::types::U64>("eth_blockNumber", ())
            .await
            .expect_err("expected error");

        m.assert();
        assert!(matches!(err, JsonRpcProviderClientError::JsonRpcError(_)));
        assert_eq!(
            0,
            client.requests_enqueued.load(Ordering::SeqCst),
            "retry queue should be zero when policy says no more retries"
        );
    }

    #[async_std::test]
    async fn test_client_should_retry_on_nonretryable_json_rpc_error_if_min_retries_is_given() {
        let mut server = mockito::Server::new_async().await;

        let m = server
            .mock("POST", "/")
            .with_status(200)
            .match_body(mockito::Matcher::PartialJson(json!({"method": "eth_blockNumber"})))
            .with_body(
                r#"{
              "jsonrpc": "2.0",
              "id": 1,
              "error": {
                "message": "some message",
                "code": -32000
              }
            }"#,
            )
            .expect(2)
            .create();

        let client = JsonRpcProviderClient::new(
            &server.url(),
            SurfRequestor::default(),
            SimpleJsonRpcRetryPolicy {
                min_retries: Some(1),
                max_retries: Some(2),
                retryable_json_rpc_errors: vec![],
                initial_backoff: Duration::from_millis(100),
                ..SimpleJsonRpcRetryPolicy::default()
            },
        );

        let err = client
            .request::<_, ethers::types::U64>("eth_blockNumber", ())
            .await
            .expect_err("expected error");

        m.assert();
        assert!(matches!(err, JsonRpcProviderClientError::JsonRpcError(_)));
        assert_eq!(
            0,
            client.requests_enqueued.load(Ordering::SeqCst),
            "retry queue should be zero when policy says no more retries"
        );
    }

    #[async_std::test]
    async fn test_client_should_retry_on_malformed_json_rpc_error() {
        let mut server = mockito::Server::new_async().await;

        let m = server
            .mock("POST", "/")
            .with_status(200)
            .match_body(mockito::Matcher::PartialJson(json!({"method": "eth_blockNumber"})))
            .with_body(
                r#"{
              "jsonrpc": "2.0",
              "error": {
                "message": "some message",
                "code": -32600
              }
            }"#,
            )
            .expect(3)
            .create();

        let client = JsonRpcProviderClient::new(
            &server.url(),
            SurfRequestor::default(),
            SimpleJsonRpcRetryPolicy {
                max_retries: Some(2),
                retryable_json_rpc_errors: vec![-32600],
                initial_backoff: Duration::from_millis(100),
                ..SimpleJsonRpcRetryPolicy::default()
            },
        );

        let err = client
            .request::<_, ethers::types::U64>("eth_blockNumber", ())
            .await
            .expect_err("expected error");

        m.assert();
        assert!(matches!(err, JsonRpcProviderClientError::SerdeJson { .. }));
        assert_eq!(
            0,
            client.requests_enqueued.load(Ordering::SeqCst),
            "retry queue should be zero when policy says no more retries"
        );
    }

    // Requires manual implementation, because mockall does not work well with generic methods
    // in non-generic traits.
    #[derive(Debug)]
    struct NullHttpPostRequestor;

    #[async_trait]
    impl HttpRequestor for NullHttpPostRequestor {
        async fn http_query<T>(&self, _: Method, _: &str, _: Option<T>) -> Result<Box<[u8]>, HttpRequestError>
        where
            T: Serialize + Send + Sync,
        {
            Err(HttpRequestError::UnknownError("use of NullHttpPostRequestor".into()))
        }
    }

    #[test_log::test(async_std::test)]
    async fn test_client_from_file() -> anyhow::Result<()> {
        let block_time = Duration::from_millis(1100);
        let snapshot_file = NamedTempFile::new()?;

        let anvil = create_anvil(Some(block_time));
        {
            let client = JsonRpcProviderClient::new(
                &anvil.endpoint(),
                SnapshotRequestor::new(SurfRequestor::default(), snapshot_file.path().to_str().unwrap()),
                SimpleJsonRpcRetryPolicy::default(),
            );

            let mut last_number = 0;

            for _ in 0..3 {
                sleep(block_time).await;

                let number: ethers::types::U64 = client.request("eth_blockNumber", ()).await?;

                assert!(number.as_u64() > last_number, "next block number must be greater");
                last_number = number.as_u64();
            }
        }

        {
            let client = JsonRpcProviderClient::new(
                &anvil.endpoint(),
                SnapshotRequestor::new(NullHttpPostRequestor, snapshot_file.path().to_str().unwrap())
                    .load(true)
                    .await,
                SimpleJsonRpcRetryPolicy::default(),
            );

            let mut last_number = 0;
            for _ in 0..3 {
                sleep(block_time).await;

                let number: ethers::types::U64 = client.request("eth_blockNumber", ()).await?;

                assert!(number.as_u64() > last_number, "next block number must be greater");
                last_number = number.as_u64();
            }
        }

        Ok(())
    }
}
