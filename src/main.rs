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
use std::cell::UnsafeCell;
use std::convert::Infallible;
use std::future::Future;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::Acquire;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::Ordering::Release;
use std::sync::atomic::{AtomicBool, AtomicUsize, fence};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread::{self, Thread};
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

    /// This one works as it should tho
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
                    sleep: tokio::time::sleep(self.delay_duration), // sleep countdown begins from here apperently
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

pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

unsafe impl<T> Sync for SpinLock<T> where T: Send {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }
    pub fn lock(&'_ self) -> Guard<'_, T> {
        while self.locked.swap(true, Acquire) {
            std::hint::spin_loop();
        }
        Guard { lock: self }
    }
    /// Safety: The &mut T from lock() must be gone!
    /// (And no cheating by keeping reference to fields of that T around!)
    pub unsafe fn unlock(&self) {
        self.locked.store(false, Release);
    }
}

// Forthe SpinLock
pub struct Guard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // Safety: The very existence of this Guard
        // guarantees we've exclusively locked the lock.
        unsafe { &*self.lock.value.get() }
    }
}
impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // Safety: The very existence of this Guard
        // guarantees we've exclusively locked the lock.
        unsafe { &mut *self.lock.value.get() }
    }
}

// Only way of unlocking the SpinLock is by dropping the Guard
impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Release);
    }
}

/// Can be done with an Arc, which even tho more convenient, allocates memory for the hidden channel
/// The way we did do it leaves it up to the user to allocate everything that needs to be allocated
/// Which is then shared via borrowing
///
/// The reduction in convenience compared to the Arc-based version is quite minimal:
/// we only needed one more line to manually create a Channel object. Note, however,
/// how the channel has to be created before the scope, to prove to the compiler that its
/// existence will outlast both the sender and receiver.
///
/// Alternate implementation with hidden Arc allocation:
///
/// pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
///     let a = Arc::new(Channel {
///         message: UnsafeCell::new(MaybeUninit::uninit()),
///         ready: AtomicBool::new(false),
///     });
///     (Sender { channel: a.clone() }, Receiver { channel: a })
/// }
pub struct Channel<T> {
    message: UnsafeCell<MaybeUninit<T>>,
    ready: AtomicBool,
}
unsafe impl<T> Sync for Channel<T> where T: Send {}
pub struct Sender<'a, T> {
    channel: &'a Channel<T>,
    receiving_thread: Thread,
}
pub struct Receiver<'a, T> {
    channel: &'a Channel<T>,
    /// If the Receiver object is sent between threads.
    /// The Sender would be unaware of that and would still refer to the
    /// thread that originally held the Receiver.
    ///
    /// A PhantomData<*const ()> does the job, since a raw
    /// pointer, such as *const (), does not implement Send  
    _no_send: PhantomData<*const ()>,
}

/// To see the compiler’s borrow checker in action, try adding a second call to channel.split()
/// in various places. You’ll see that calling it a second time within the
/// thread scope results in an error, while calling it after the scope is acceptable. Even
/// calling split() before the scope is fine, as long as you stop using the returned Sender
/// and Receiver before the scope starts
impl<T> Channel<T> {
    pub const fn new() -> Self {
        Self {
            message: UnsafeCell::new(MaybeUninit::uninit()),
            ready: AtomicBool::new(false),
        }
    }
    // Lifetimes can be elided, but aren't, to more easily illustrate that what we gave is what we got
    pub fn split<'a>(&'a mut self) -> (Sender<'a, T>, Receiver<'a, T>) {
        *self = Self::new();
        (
            Sender {
                channel: self, // coerced into &*self
                receiving_thread: thread::current(),
            },
            Receiver {
                channel: self, // coerced into &*self
                _no_send: PhantomData,
            },
        )
    }
}

impl<T> Sender<'_, T> {
    pub fn send(self, message: T) {
        unsafe { (*self.channel.message.get()).write(message) };
        self.channel.ready.store(true, Release);
        self.receiving_thread.unpark();
    }
}

impl<T> Receiver<'_, T> {
    /// Only the thread that calls split() may call receive()
    pub fn receive(self) -> T {
        // Remember that thread::park() might return spuriously. (Or
        // because something other than our send method called unpark().)
        // This means that we cannot assume that the ready flag has been set
        // when park() returns. So, we need to use a loop to check the flag
        // again after getting unparked.
        while !self.channel.ready.swap(false, Acquire) {
            thread::park();
        }
        unsafe { (*self.channel.message.get()).assume_init_read() }
    }
}

impl<T> Drop for Channel<T> {
    fn drop(&mut self) {
        if *self.ready.get_mut() {
            unsafe { self.message.get_mut().assume_init_drop() }
        }
    }
}

/// A simple example showing how to use `Channel`, `Sender`, and `Receiver`.
///
/// # Examples
///
/// ```
/// use std::thread;
/// use your_crate::Channel;
///
/// let mut channel = Channel::new();
///
/// thread::scope(|s| {
///     let (sender, receiver) = channel.split();
///
///     s.spawn(move || {
///         sender.send("hello world!");
///     });
///
///     assert_eq!(receiver.receive(), "hello world!");
/// });
/// ```

struct ArcData<T> {
    /// Counts only allocations to T (strong count)
    data_ref_count: AtomicUsize,
    /// Counts all the allocations to ArcData (strong + weak count)
    /// so when strong count is 0 the Arc isn't dropped but it's data is now None
    /// Also this makes it easier to check if all references strong and weak are gone
    /// In one atomic operation, rather than checking tow separate countrs
    alloc_ref_count: AtomicUsize,
    data: UnsafeCell<Option<T>>,
}

pub struct ArcToo<T> {
    //ptr: NonNull<ArcData<T>>,
    weak: Weak<T>,
}

unsafe impl<T: Send + Sync> Send for ArcToo<T> {}
unsafe impl<T: Send + Sync> Sync for ArcToo<T> {}

impl<T> ArcToo<T> {
    pub fn new(data: T) -> ArcToo<T> {
        ArcToo {
            weak: Weak {
                ptr: NonNull::from(Box::leak(Box::new(ArcData {
                    alloc_ref_count: AtomicUsize::new(1),
                    data_ref_count: AtomicUsize::new(1),
                    data: UnsafeCell::new(Some(data)),
                }))),
            },
        }
    }

    // fn data(&self) -> &ArcData<T> {
    //     unsafe { self.ptr.as_ref() }
    // }

    pub fn get_mut(arc: &mut Self) -> Option<&mut T> {
        if arc.weak.data().alloc_ref_count.load(Relaxed) == 1 {
            fence(Acquire);
            // Safety: Nothing else can access the data, since
            // there's only one Arc, to which we have exclusive access,
            // and no Weak pointers.
            let arcdata = unsafe { arc.weak.ptr.as_mut() };
            let option = arcdata.data.get_mut();
            // We know the data is still available since we
            // have an Arc to it, so this won't panic.
            let data = option.as_mut().unwrap();
            Some(data)
        } else {
            None
        }
    }

    pub fn downgrade(arc: &Self) -> Weak<T> {
        arc.weak.clone()
    }
}

impl<T> Deref for ArcToo<T> {
    type Target = T;

    fn deref(&self) -> &T {
        let ptr = self.weak.data().data.get();
        // Safety: Since there's an Arc to the data,
        // the data exists and may be shared.
        unsafe { (*ptr).as_ref().unwrap() }
    }
}

impl<T> Clone for ArcToo<T> {
    fn clone(&self) -> Self {
        // We’ll simply use self.weak.clone() to reuse the code above for the first (weak)
        // counter, so we only have to manually increment the second counter (strong)
        let weak = self.weak.clone();
        if weak.data().data_ref_count.fetch_add(1, Relaxed) > usize::MAX / 2 {
            std::process::abort();
        }
        ArcToo { weak }
    }
}

impl<T> Drop for ArcToo<T> {
    fn drop(&mut self) {
        if self.weak.data().data_ref_count.fetch_sub(1, Release) == 1 {
            fence(Acquire);
            let ptr = self.weak.data().data.get();
            // Safety: The data reference counter is zero,
            // so nothing will access it.
            unsafe {
                (*ptr) = None;
            }
        }
    }
}

pub struct Weak<T> {
    ptr: NonNull<ArcData<T>>,
}

unsafe impl<T: Sync + Send> Send for Weak<T> {}
unsafe impl<T: Sync + Send> Sync for Weak<T> {}

impl<T> Weak<T> {
    fn data(&self) -> &ArcData<T> {
        unsafe { self.ptr.as_ref() }
    }

    pub fn upgrade(&self) -> Option<ArcToo<T>> {
        let mut n = self.data().data_ref_count.load(Relaxed);
        loop {
            if n == 0 {
                return None;
            }
            assert!(n < usize::MAX);
            if let Err(e) =
                self.data()
                    .data_ref_count
                    .compare_exchange_weak(n, n + 1, Relaxed, Relaxed)
            {
                n = e;
                continue;
            }
            return Some(ArcToo { weak: self.clone() });
        }
    }
}

impl<T> Clone for Weak<T> {
    fn clone(&self) -> Self {
        if self.data().alloc_ref_count.fetch_add(1, Relaxed) > usize::MAX / 2 {
            std::process::abort();
        }
        Weak { ptr: self.ptr }
    }
}

impl<T> Drop for Weak<T> {
    fn drop(&mut self) {
        if self.data().alloc_ref_count.fetch_sub(1, Release) == 1 {
            fence(Acquire);
            unsafe {
                drop(Box::from_raw(self.ptr.as_ptr()));
            }
        }
    }
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
