use http_body_util::{BodyExt, combinators::BoxBody};
use http_body_util::{Empty, Full};
use hyper::body::Frame;
use hyper::body::{Body, Bytes};
use hyper::server::conn::http1;
use hyper::{Method, StatusCode};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use local_ip_address;
use pin_project::pin_project;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Sleep;
use tower::{Service, ServiceBuilder};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

async fn echo(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => Ok(Response::new(full("Try POSTing data to /echo"))),
        (&Method::POST, "/echo") => Ok(Response::new(req.into_body().boxed())),
        (&Method::POST, "/echo/uppercase") => {
            let frame_stream = req.into_body().map_frame(|frame| {
                let frame = if let Ok(data) = frame.into_data() {
                    data.iter()
                        .map(|byte| byte.to_ascii_uppercase())
                        .collect::<Bytes>()
                } else {
                    Bytes::new()
                };

                Frame::data(frame)
            });

            Ok(Response::new(frame_stream.boxed()))
        }
        (&Method::POST, "/echo/reversed") => {
            let upper = req.body().size_hint().upper().unwrap_or(u64::MAX);
            if upper > 1024 * 64 {
                let mut resp = Response::new(full("Body too big"));
                *resp.status_mut() = hyper::StatusCode::PAYLOAD_TOO_LARGE;
                return Ok(resp);
            }

            let whole_body = req.collect().await?.to_bytes();
            let reversed_body = whole_body.iter().rev().cloned().collect::<Vec<u8>>();

            Ok(Response::new(full(reversed_body)))
        }
        _ => {
            let mut not_found = Response::new(empty());
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            println!("Not found: {} {}", req.method(), req.uri().path());
            Ok(not_found)
        }
    }
}

fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
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
    S: Service<R, Response = Response<BoxBody<Bytes, hyper::Error>>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    R: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = SpawnRequestFuture<S::Response, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let future = self.inner.call(req);

        // Spawn the entire service call chain in a new task
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

impl<S> RateLimit<S> {
    /// This constructor allows each connection to have its own rate limiting counter.
    /// Since http1 requires each connection request to produce a response before it takes another request,
    /// the unique counter never goes up past 1 for any connection. 
    /// Basically a no op for http1
    pub fn new(inner: S, request_per_delay: usize, delay: Duration, address: SocketAddr) -> Self {
        Self {
            inner,
            request_per_delay,
            delay_duration: delay,
            current_count: Arc::new(Mutex::new(0)),
            address,
        }
    }

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

    fn check_rate_limit(&self) -> RateLimitDecision {
        let mut count = self.current_count.lock().unwrap();

        if *count >= self.request_per_delay * 3 {
            println!(
                "[RateLimit] REJECTING request. Current count: {}, from: {}",
                *count, self.address
            );
            RateLimitDecision::Reject
        } else if *count >= self.request_per_delay + 1 {
            *count += 1;
            println!(
                "[RateLimit] DELAYING request. Current count: {}, from: {}",
                *count, self.address
            );
            RateLimitDecision::Delay(self.current_count.clone())
        } else {
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
    S: Service<R, Response = Response<BoxBody<Bytes, hyper::Error>>> + Clone + Send + 'static,
    R: Send + 'static,
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
                let count_clone = count.clone();
                let delay = self.delay_duration;

                // This essentially does nothing but give more time to better simulate the work of the middleware

                // tokio::task::spawn(async move {
                //     tokio::time::sleep(delay).await;
                //     let mut guard = count_clone.lock().unwrap();
                //     *guard -= 1;
                //     //println!("[RateLimit] Decrementing counter after delay window");
                // });

                RateLimitFuture::Delayed {
                    sleep: tokio::time::sleep(self.delay_duration),
                    inner: Some(self.inner.call(req)),
                    count: count_clone,
                }
            }
            RateLimitDecision::Allow(count) => {
                let count_clone = count.clone();
                let delay = self.delay_duration;

                // This essentially does nothing but give more time to better simulate the work of the middleware

                // tokio::task::spawn(async move {
                //     tokio::time::sleep(delay).await;
                //     let mut guard = count_clone.lock().unwrap();
                //     *guard -= 1;
                //     //println!("[RateLimit] Decrementing counter after rate limit window");
                // });

                RateLimitFuture::Processing {
                    inner: self.inner.call(req),
                    count: count_clone,
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
                        self.set(RateLimitFuture::Processing {
                            inner: inner_fut,
                            count: count_clone,
                        });

                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                },

                RateLimitFutureProj::Processing { inner, count } => match inner.poll(cx) {
                    Poll::Ready(result) => {
                        let mut guard = count.lock().unwrap();
                        *guard -= 1;

                        return Poll::Ready(result);
                    }
                    Poll::Pending => return Poll::Pending,
                },
            }
        }
    }
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C signal handler");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let port = 7878;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let global_count = Arc::new(Mutex::new(0));

    let listener = TcpListener::bind(addr).await.unwrap();

    let mut help_ip = String::from("oops");

    match local_ip_address::local_ip() {
        Ok(ip) => {
            help_ip = ip.to_string();
            println!("Local IP address: {}:{}", ip, port);
        }
        Err(e) => eprintln!("Error: {}", e),
    }

    let graceful = Arc::new(hyper_util::server::graceful::GracefulShutdown::new());
    let mut signal = std::pin::pin!(shutdown_signal());

    // Client
    tokio::task::spawn(async move {
        let uri_str = format!("http://{}:{}", help_ip, port);

        let uri: hyper::Uri = uri_str.parse().expect("Invalid URI");

        let host = uri.host().expect("Uri has no host");
        let port = uri.port_u16().unwrap_or(8080);

        let address = format!("{}:{}", host, port);

        println!("Client prepared for host={}:{}", host, port);

        let stream = TcpStream::connect(address).await;
    });

    loop {
        tokio::select! {
            Ok((stream, address)) = listener.accept() => {
                let io = TokioIo::new(stream);
                let global_count_clone = global_count.clone();

                let graceful_clone = graceful.clone();

                tokio::task::spawn(async move {
                    println!("Accepted connection from: {}", address);

                    let delay = Duration::from_millis(1000);
                    let requests_per_delay = 2;

                    let svc = tower::service_fn(echo);
                    let svc = ServiceBuilder::new()
                        .layer_fn(SpawnRequest::new)
                        .layer_fn(Logger::new)
                        .layer_fn(move |inner| {
                            RateLimit::with_shared_counter(inner, requests_per_delay, delay, address, global_count_clone.clone())
                        })
                        .service(svc);

                    let svc = TowerToHyperService::new(svc);

                    let conn = http1::Builder::new().serve_connection(io, svc);

                    let shutdown = graceful_clone.watch(conn);

                    if let Err(err) = shutdown.await {
                        eprintln!("Server error serving connection: {}", err);
                    }
                });
            },

            _ = &mut signal => {
                drop(listener);
                eprintln!("Graceful shutdown signal received");
                break;
            },
        }
    }

    Ok(())
}

// Option A — Use HTTP/2 (multiplexing streams)

// HTTP/2 allows many independent request/response streams over the same TCP connection; the server can process them concurrently and send frames interleaved.
// If you want concurrent requests over one connection, use HTTP/2. Hyper supports HTTP/2; you can:
// enable TLS and let clients negotiate h2, or
// implement h2c (HTTP/2 over cleartext) if you control client and server (a bit more work).
// This is the standard, correct solution for per-connection parallelism.


// Option B — Use multiple TCP connections (load-tool style)

// Clients open N connections (e.g. hey -c N), each connection processed concurrently by your server. This is what most load-testers do by default to generate concurrency on HTTP/1.1.
// Simple and effective for testing and many real-world setups (browsers open multiple connections).


// Option C — Implement a request-queue + response-serializer (hard)

// You could accept requests on the connection, spawn background tasks to handle them, and then serialize responses back onto the connection in the correct order.
// This is effectively reimplementing a multiplexing/streaming protocol and is complex and error-prone (you must obey HTTP semantics, handle ordering, backpressure, chunking, connection flow-control, etc.). Not recommended unless you need that exact behavior and know what you’re doing.


// Option D — Switch protocol (WebSocket / custom multiplexing)

// Use WebSocket or a custom protocol that allows multiple concurrent logical requests on one TCP connection. Also requires client changes.


// Option E — Pipelining

// HTTP/1.1 pipelining lets a client send multiple requests over the same connection without waiting for each response.
// The server reads requests in order and can start processing each in a separate task, though responses still must be sent in order.
// To implement it, you’d buffer incoming requests from the connection, spawn a task for each to run your middleware stack,
// and then write responses back to the stream in the original request order.
// This allows your SpawnRequest middleware to actually run requests concurrently instead of sequentially.


// Option F - HTTP streaming or chunked transfer encoding

// Instead of waiting for the full response before sending it, the server can first send the headers and then gradually stream the body in chunks.
// This lets the client start receiving data immediately, and your server can start processing large bodies piece by piece rather than buffering everything in memory.
// In practice, you’d use a Body type that implements Stream<Item = Bytes> and push chunks as they’re ready.