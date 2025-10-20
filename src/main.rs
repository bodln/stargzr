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
use std::alloc::LayoutError;
use std::convert::Infallible;
use std::error::Error;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpListener;
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
            // Map this body's frame to a different type
            let frame_stream = req.into_body().map_frame(|frame| {
                let frame = if let Ok(data) = frame.into_data() {
                    // Convert every byte in every Data frame to uppercase
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
        // Yet another route inside our match block...
        (&Method::POST, "/echo/reversed") => {
            // Protect our server from massive bodies.
            let upper = req.body().size_hint().upper().unwrap_or(u64::MAX);
            if upper > 1024 * 64 {
                let mut resp = Response::new(full("Body too big"));
                *resp.status_mut() = hyper::StatusCode::PAYLOAD_TOO_LARGE;
                return Ok(resp);
            }

            // Await the whole body to be collected into a single `Bytes`...
            let whole_body = req.collect().await?.to_bytes();

            // Iterate the whole body in reverse order and collect into a new Vec.
            let reversed_body = whole_body.iter().rev().cloned().collect::<Vec<u8>>();

            Ok(Response::new(full(reversed_body)))
        }
        // Return 404 Not Found for other routes.
        _ => {
            let mut not_found = Response::new(empty());
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            println!("Not found: {} {}", req.method(), req.uri().path());
            Ok(not_found)
        }
    }
}

// We create some utility functions to make Empty and Full bodies
// fit our broadened Response body type.
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
// R should be concrete here so we can read its fields, in this case Request<Incoming>
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
pub struct RateLimit<S> {
    inner: S,
    request_per_delay: usize,
    delay_duration: Duration,
    current_count: Arc<Mutex<usize>>,
    address: SocketAddr,
}

impl<S> RateLimit<S> {
    pub fn new(inner: S, request_per_delay: usize, address: SocketAddr) -> Self {
        Self {
            inner,
            request_per_delay,
            delay_duration: Duration::from_secs(1),
            current_count: Arc::new(Mutex::new(0)),
            address: address,
        }
    }

    pub fn with_shared_counter(
        inner: S,
        request_per_delay: usize,
        address: SocketAddr,
        shared_counter: Arc<Mutex<usize>>,
    ) -> Self {
        Self {
            inner,
            request_per_delay,
            delay_duration: Duration::from_secs(1),
            address: address,
            current_count: shared_counter,
        }
    }

    fn check_rate_limit(&self) -> RateLimitDecision {
        let mut count = self.current_count.lock().unwrap();

        if *count >= self.request_per_delay * 3 {
            println!("[RateLimit] REJECTING request. Current count: {}, from: {}", *count, self.address);
            RateLimitDecision::Reject
        } else if *count >= self.request_per_delay + 1 {
            *count += 1;
            println!("[RateLimit] DELAYING request. Current count: {}, from: {}", *count, self.address);
            RateLimitDecision::Delay(self.current_count.clone())
        } else {
            *count += 1;
            println!("[RateLimit] ALLOWING request. Current count: {}, from: {}", *count, self.address);
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
            RateLimitDecision::Delay(count) => RateLimitFuture::Delayed {
                sleep: tokio::time::sleep(self.delay_duration),
                inner: Some(self.inner.call(req)),
                count: count.clone(),
            },
            RateLimitDecision::Allow(count) => RateLimitFuture::Processing {
                inner: self.inner.call(req),
                count: count.clone(),
            },
        }
    }
}

#[pin_project(project = RateLimitFutureProj)]
pub enum RateLimitFuture<F, Resp, E> {
    Rejected(#[pin] std::future::Ready<Result<Resp, E>>),
    Delayed {
        #[pin]
        sleep: Sleep,
        // We dont pin this because we only pass it through to the next state
        inner: Option<F>,
        count: Arc<Mutex<usize>>,
    },
    Processing {
        #[pin]
        inner: F,
        count: Arc<Mutex<usize>>,
    },
    Waiting {
        #[pin]
        sleep: Sleep,
        result: Option<Result<Resp, E>>,
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
                } => {
                    // Poll the delay first
                    match sleep.poll(cx) {
                        Poll::Ready(()) => {
                            println!("[RateLimit] Delay complete, processing request");
                            let inner_fut = inner.take().expect("polled after completion");
                            let clone_count = count.clone();
                            self.set(RateLimitFuture::Processing {
                                inner: inner_fut,
                                count: clone_count,
                            });

                            continue;
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }

                RateLimitFutureProj::Processing { inner, count } => {
                    match inner.poll(cx) {
                        Poll::Ready(res) => {
                            // Instead of returning immediately, wait a short duration
                            let sleep = tokio::time::sleep(Duration::from_millis(200)); // adjust as needed
                            let clone_count = count.clone();
                            self.set(RateLimitFuture::Waiting {
                                sleep,
                                result: Some(res),
                                count: clone_count,
                            });
                            continue;
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }

                RateLimitFutureProj::Waiting {
                    sleep,
                    result,
                    count,
                } => {
                    match sleep.poll(cx) {
                        Poll::Ready(()) => {
                            let mut guard = count.lock().unwrap();
                            *guard -= 1; // decrement after delay
                            return Poll::Ready(result.take().unwrap());
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct AsyncRequest<F> {
    inner: F,
}

// maybe try
// run this layer first
// have it create a task that polls inner 
// each task returns its result to a channel (mpsc or oneshot)
// then we somehow display and coordinate reusls as they come 
// maybe in creating the middleware struct we create a task with the consumer that returns stuff idk

impl<F> AsyncRequest<F> {
    pub fn new(inner: F) -> Self{
        Self { inner: inner }
    }
}

impl<F, R> Service<R> for AsyncRequest<F>
where
    F: Service<R, Response = Response<BoxBody<Bytes, hyper::Error>>> + Clone + Send + 'static,
    F::Future: Send + 'static,
    F::Error: Send + 'static,
    R: Send + 'static,
{
    type Response = F::Response;

    type Error = F::Error;

    type Future = AsyncRequestFuture<F::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let future = self.inner.call(req);
        AsyncRequestFuture { inner: future }
    }
}

#[pin_project(project = AsyncRequestFutureProj)]
pub struct AsyncRequestFuture<F> {
    #[pin]
    inner: F,
}

impl<F, Resp, E> Future for AsyncRequestFuture<F> 
where 
    F: Future<Output = Result<Resp, E>> + Send + 'static,
    Resp: Send + 'static,
    E: Send + 'static,
{
    type Output = Result<Resp, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        
        this.inner.poll(cx)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let port = 7878;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let global_count = Arc::new(Mutex::new(0));

    // We create a TcpListener and bind it
    let listener = TcpListener::bind(addr).await?;

    match local_ip_address::local_ip() {
        Ok(ip) => println!("Local IP address: {}:{}", ip, port),
        Err(e) => eprintln!("Error: {}", e),
    }

    // We start a loop to continuously accept incoming connections
    loop {
        let (stream, address) = listener.accept().await?;

        // Use an adapter to access something implementing `tokio::io` traits as if they implement
        // `hyper::rt` IO traits.
        let io = TokioIo::new(stream);

        let global_count_clone = global_count.clone();

        tokio::task::spawn(async move {
            print!("Accepted connection from: {}\n", address);

            let svc = tower::service_fn(echo);
            let svc = ServiceBuilder::new()
                .layer_fn(AsyncRequest::new)
                .layer_fn(Logger::new)
                .layer_fn(move |inner| RateLimit::with_shared_counter(inner, 1, address.clone(), global_count_clone.clone())) // global rate limit
                //.layer_fn(|inner| RateLimit::new(inner, 1, address.clone()))
                .service(svc);

            let svc = TowerToHyperService::new(svc);

            if let Err(err) = http1::Builder::new().serve_connection(io, svc).await {
                eprintln!("server error: {}", err);
            }
        });
    }
}
