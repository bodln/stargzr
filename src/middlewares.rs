use http_body_util::{BodyExt, combinators::BoxBody};
use http_body_util::{Empty, Full};
use hyper::{Response, StatusCode, Method, Request};
use hyper::body::{Bytes, Body, Frame};
use pin_project::pin_project;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::Sleep;
use tower::Service;

// Main request handler - routes incoming HTTP requests
// Takes: Request with incoming body stream
// Returns: Response with a BoxBody (type-erased body that streams Bytes chunks)
pub async fn echo(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    // Pattern match on (method, path) for manual routing
    match (req.method(), req.uri().path()) {
        // GET / - Return simple text response
        (&Method::GET, "/") => {
            // full() creates a body with all data at once (buffered, not streamed)
            Ok(Response::new(full("Try POSTing data to /echo")))
        }

        // POST /echo - Echo the request body back (streaming, no buffering)
        (&Method::POST, "/echo") => {
            // into_body() gives us the body stream, boxed() type-erases it
            // This streams frames through as they arrive - no memory buffering
            Ok(Response::new(req.into_body().boxed()))
        }

        // POST /echo/uppercase - Transform body to uppercase while streaming
        (&Method::POST, "/echo/uppercase") => {
            // map_frame() transforms each chunk as it arrives (streaming transformation)
            let frame_stream = req.into_body().map_frame(|frame| {
                // Extract data from the frame (or empty if it's trailers/metadata)
                let frame = if let Ok(data) = frame.into_data() {
                    // data is one Bytes chunk, transform each byte to uppercase
                    data.iter()
                        .map(|byte| byte.to_ascii_uppercase())
                        .collect::<Bytes>()
                } else {
                    Bytes::new()
                };
                // Wrap back into a data frame
                Frame::data(frame)
            });

            Ok(Response::new(frame_stream.boxed()))
        }

        // POST /echo/reversed - Reverse the entire body (requires buffering)
        (&Method::POST, "/echo/reversed") => {
            // Check estimated body size to prevent OOM attacks
            let upper = req.body().size_hint().upper().unwrap_or(u64::MAX);
            if upper > 1024 * 64 {
                let mut resp = Response::new(full("Body too big"));
                *resp.status_mut() = hyper::StatusCode::PAYLOAD_TOO_LARGE;
                return Ok(resp);
            }

            // collect() waits for ALL frames and concatenates them into memory
            // This is buffering - we need the entire body to reverse it
            let whole_body = req.collect().await?.to_bytes();
            let reversed_body = whole_body.iter().rev().cloned().collect::<Vec<u8>>();

            Ok(Response::new(full(reversed_body)))
        }

        // 404 for all other routes
        _ => {
            let mut not_found = Response::new(empty());
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            println!("Not found: {} {}", req.method(), req.uri().path());
            Ok(not_found)
        }
    }
}

// Creates empty body (0 bytes) - used for 404s
pub fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {}) // Empty can never error
        .boxed() // Type-erase into BoxBody
}

// Creates body with all data at once - used for small responses
// Accepts &str, String, Vec<u8>, or Bytes
pub fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {}) // Full can never error
        .boxed() // Type-erase into BoxBody
}

#[derive(Debug, Clone)]
pub struct Logger<S> {
    inner: S,
}

impl<S> Logger<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S, R> Service<R> for Logger<S>
where
    S: Service<R> + Clone,
{
    type Error = S::Error;
    type Future = S::Future;
    type Response = S::Response;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        println!("Request received and logged.");
        self.inner.call(req)
    }
}

#[derive(Clone)]
pub struct SpawnRequest<S> {
    inner: S,
}

impl<S> SpawnRequest<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S, R> Service<R> for SpawnRequest<S>
where
    S: Service<R> + Clone + Send + 'static,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    //R: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = SpawnRequestFuture<S::Response, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let future = self.inner.call(req);

        // Spawn the entire service call chain in a new task, which doesnt improve anything right now with http1
        let handle = tokio::task::spawn(async move { future.await });

        SpawnRequestFuture { handle }
    }
}

#[pin_project]
pub struct SpawnRequestFuture<Resp, E> {
    #[pin]
    handle: tokio::task::JoinHandle<Result<Resp, E>>,
}

impl<Resp, E> Future for SpawnRequestFuture<Resp, E> {
    type Output = Result<Resp, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        match this.handle.poll(cx) {
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(join_error)) => {
                // Task panicked or was cancelled - this is unrecoverable
                panic!("Request task panicked: {:?}", join_error);
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Clone)]
pub struct RateLimit<S> {
    inner: S,
    request_per_delay: usize,
    delay_duration: Duration,
    current_count: Arc<Mutex<usize>>,
    address: SocketAddr,
}

// To properly test this we need to do something like hey -n 10 -c 2 http://192.168.1.2:7878/
// -c 2 because 1 apparently isnt enough to challenge it so we dont get any reject messages.
impl<S> RateLimit<S> {
    /// Creates a rate limiter with a unique counter per connection.
    /// NOTE: This is essentially a no-op for HTTP/1.1 because each connection
    /// processes requests sequentially, so the counter never goes above 1.
    /// Use `with_shared_counter` instead for effective rate limiting across connections.
    pub fn new(inner: S, request_per_delay: usize, delay: Duration, address: SocketAddr) -> Self {
        Self {
            inner,
            request_per_delay,
            delay_duration: delay,
            current_count: Arc::new(Mutex::new(0)),
            address,
        }
    }

    /// Creates a rate limiter that shares a counter across all connections.
    /// This is what you want for real rate limiting - it tracks all requests
    /// from an IP address across all their connections, preventing clients
    /// from bypassing the rate limit by opening multiple connections.
    pub fn with_shared_counter(
        inner: S,
        request_per_delay: usize,
        delay: Duration,
        address: SocketAddr,
        shared_counter: Arc<Mutex<usize>>,
    ) -> Self {
        Self {
            inner,
            request_per_delay,
            delay_duration: delay,
            address,
            current_count: shared_counter,
        }
    }

    /// Decides what to do with an incoming request based on current load.
    /// This implements a three-tier token bucket algorithm:
    /// 1. Allow: We have capacity, increment counter and proceed
    /// 2. Delay: We're over capacity but not critically, add artificial delay
    /// 3. Reject: We're critically over capacity, reject immediately
    ///
    /// The thresholds demonstrate escalating back-pressure:
    /// - Normal operation: count stays below request_per_delay + 1
    /// - Moderate overload: count between request_per_delay + 1 and request_per_delay * 3
    /// - Severe overload: count >= request_per_delay * 3
    fn check_rate_limit(&self) -> RateLimitDecision {
        let mut count = self.current_count.lock().unwrap();

        // Reject threshold: 3x over the limit means we're being hammered
        if *count >= self.request_per_delay * 3 {
            println!(
                "[RateLimit] REJECTING request. Current count: {}, from: {}",
                *count, self.address
            );
            RateLimitDecision::Reject
        }
        // Delay threshold: We're over the limit but not critically
        // Add artificial delay to slow the client down gradually
        else if *count >= self.request_per_delay + 1 {
            *count += 1;
            println!(
                "[RateLimit] DELAYING request. Current count: {}, from: {}",
                *count, self.address
            );
            RateLimitDecision::Delay(self.current_count.clone())
        }
        // Allow: We have capacity in our token bucket
        else {
            *count += 1;
            println!(
                "[RateLimit] ALLOWING request. Current count: {}, from: {}",
                *count, self.address
            );
            RateLimitDecision::Allow(self.current_count.clone())
        }
    }
}

#[derive(Debug)]
enum RateLimitDecision {
    Reject,
    Delay(Arc<Mutex<usize>>),
    Allow(Arc<Mutex<usize>>),
}

impl<S, R> Service<R> for RateLimit<S>
where
    // Response = Response<BoxBody<Bytes, hyper::Error>> because it ensures that whats returned from inner is in the same format as the response i am making by hand
    // By constraining S::Response = Response<BoxBody<Bytes, hyper::Error>>, we:
    // Unify the type for both the inner service and the manually created response.
    // The compiler can verify that all enum variants return the same Response type.
    S: Service<R, Response = Response<BoxBody<Bytes, hyper::Error>>> + Clone + Send + 'static,
    // R: Send + 'static, // no need for it to be send because we arent moving it anywhere
{
    type Error = S::Error;
    type Response = S::Response;
    type Future = RateLimitFuture<S::Future, S::Response, S::Error>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        match self.check_rate_limit() {
            RateLimitDecision::Reject => {
                println!("Rejected a request");
                let mut resp = Response::new(full("Rate limit exceeded"));
                *resp.status_mut() = StatusCode::TOO_MANY_REQUESTS;
                RateLimitFuture::Rejected(std::future::ready(Ok(resp)))
            }
            RateLimitDecision::Delay(count) => {
                let count_for_spawn = count.clone();
                let delay = self.delay_duration;

                // This is the heart of the token bucket algorithm!
                // We spawn a background task that will "return a token to the bucket"
                // after delay_duration has elapsed. This ensures our counter represents
                // "requests in the last N seconds" rather than "requests being processed right now"
                tokio::task::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let mut guard = count_for_spawn.lock().unwrap();
                    *guard = guard.saturating_sub(1); // Return one token to the bucket
                });

                RateLimitFuture::Delayed {
                    sleep: tokio::time::sleep(self.delay_duration),
                    inner: Some(self.inner.call(req)),
                    count: count.clone(),
                }
            }
            RateLimitDecision::Allow(count) => {
                let count_for_spawn = count.clone();
                let delay = self.delay_duration;

                // Same token bucket mechanism as above.
                // Even though we're allowing this request immediately,
                // we still need to track that it happened for the next delay_duration period.
                tokio::task::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let mut guard = count_for_spawn.lock().unwrap();
                    *guard = guard.saturating_sub(1); // Return one token after the time window
                });

                RateLimitFuture::Processing {
                    inner: self.inner.call(req),
                    count: count.clone(),
                }
            }
        }
    }
}

#[pin_project(project = RateLimitFutureProj)]
pub enum RateLimitFuture<F, Resp, E> {
    Rejected(#[pin] std::future::Ready<Result<Resp, E>>),
    Delayed {
        #[pin]
        sleep: Sleep,
        inner: Option<F>,
        count: Arc<Mutex<usize>>,
    },
    Processing {
        #[pin]
        inner: F,
        count: Arc<Mutex<usize>>,
    },
}

impl<F, Resp, E> Future for RateLimitFuture<F, Resp, E>
where
    F: Future<Output = Result<Resp, E>>,
{
    type Output = Result<Resp, E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match self.as_mut().project() {
                RateLimitFutureProj::Rejected(reject_future) => {
                    return reject_future.poll(cx);
                }

                RateLimitFutureProj::Delayed {
                    sleep,
                    inner,
                    count,
                } => match sleep.poll(cx) {
                    Poll::Ready(()) => {
                        println!("[RateLimit] Delay complete, processing request");
                        let inner_fut = inner.take().expect("polled after completion");
                        let count_clone = count.clone();
                        // Transition from Delayed to Processing state
                        self.set(RateLimitFuture::Processing {
                            inner: inner_fut,
                            count: count_clone,
                        });
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                },

                RateLimitFutureProj::Processing { inner, count: _ } => match inner.poll(cx) {
                    Poll::Ready(result) => {
                        // CRITICAL: We do NOT decrement the counter here!
                        // The spawned background task handles decrementing after delay_duration.
                        // This ensures the counter represents "requests in the last N seconds"
                        // rather than "currently processing requests".
                        // If we decremented here, fast requests would immediately free up
                        // their token, breaking the rate limiting time window.
                        return Poll::Ready(result);
                    }
                    Poll::Pending => return Poll::Pending,
                },
            }
        }
    }
}
