use atomic_wait::{wait, wake_all, wake_one};
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
use std::mem::{ManuallyDrop, MaybeUninit};
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::Acquire;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::Ordering::Release;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, fence};
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

// For the SpinLock
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
/// use NetworkTests::Channel;
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
    /// Number of `Arc`s.
    data_ref_count: AtomicUsize,
    /// Number of `Weak`s, plus one if there are any `Arc`s.
    /// To keep from checking for strong and weak connections during creation and destruction,
    /// we implement this counter as counting more than just weak connections so it is all one atomic operation.
    alloc_ref_count: AtomicUsize,
    /// The data. Dropped if there are only weak pointers left.
    data: UnsafeCell<ManuallyDrop<T>>,
}

pub struct ArcToo<T> {
    ptr: NonNull<ArcData<T>>,
}

unsafe impl<T: Send + Sync> Send for ArcToo<T> {}
unsafe impl<T: Send + Sync> Sync for ArcToo<T> {}

impl<T> ArcToo<T> {
    pub fn new(data: T) -> ArcToo<T> {
        ArcToo {
            ptr: NonNull::from(Box::leak(Box::new(ArcData {
                alloc_ref_count: AtomicUsize::new(1),
                data_ref_count: AtomicUsize::new(1),
                data: UnsafeCell::new(ManuallyDrop::new(data)),
            }))),
        }
    }

    fn data(&self) -> &ArcData<T> {
        unsafe { self.ptr.as_ref() }
    }

    pub fn get_mut(arc: &mut Self) -> Option<&mut T> {
        // If we had used Relaxed for the compare_exchange instead, it would have been
        // possible for the subsequent load from data_ref_count to not see the new value of
        // a freshly upgraded Weak pointer, even though the compare_exchange had already
        // confirmed that every Weak pointer had been dropped.
        // Meaning the Acqurie ordering makes sure that the dropping of all those Weaks has been made visible ot all.

        // Acquire matches Weak::drop's Release decrement, to make sure any
        // upgraded pointers are visible in the next data_ref_count.load.
        if arc
            .data()
            .alloc_ref_count
            .compare_exchange(1, usize::MAX, Acquire, Relaxed) // Make sure there are no Weaks in any thread
            .is_err()
        {
            return None;
        }

        // This is fine being Relaxed because the CAS on alloc_ref_count with Acquire
        // synchronizes with Release operations from Weak::drop. This means any thread that
        // upgraded a Weak to an Arc (incrementing data_ref_count) and then dropped its Weak
        // has published that increment, making it visible to us now. The usize::MAX barrier
        // prevents new Weak upgrades during our check. So if there are other Arcs anywhere,
        // they either: (1) were created before our CAS and are visible due to the Acquire
        // synchronization, or (2) can't be created right now because alloc_ref_count is locked.
        // Either way, a Relaxed load will see data_ref_count > 1 if we're not unique.
        let is_unique = arc.data().data_ref_count.load(Relaxed) == 1; // Checks if we are the only Arc in all threads

        // Release matches Acquire increment in `downgrade`, to make sure any
        // changes to the data_ref_count that come after `downgrade` don't
        // change the is_unique result above.
        arc.data().alloc_ref_count.store(1, Release); // Make sure we unblock the creation of new Weaks

        if !is_unique {
            return None;
        }

        // Acquire to match Arc::drop's Release decrement, to make sure nothing
        // else is accessing the data.
        fence(Acquire); // Here we make sure every release for every variable, and things before it, is visible

        unsafe { Some(&mut *arc.data().data.get()) }
    }

    pub fn downgrade(arc: &Self) -> WeakToo<T> {
        let mut n = arc.data().alloc_ref_count.load(Relaxed);

        loop {
            if n == usize::MAX {
                std::hint::spin_loop();
                n = arc.data().alloc_ref_count.load(Relaxed);

                continue;
            }

            assert!(n < usize::MAX - 1);

            // Acquire synchronises with get_mut's release-store.
            // Makes sure that any downgrade that happens is guranteed to happen after Release,
            // meaning it makes sure that we confirmed that we either definitely can or cannot make a &mut (as far as Weaks are concerned).
            // Relax ordering would not enforce these gurantees.
            if let Err(e) =
                arc.data()
                    .alloc_ref_count
                    .compare_exchange_weak(n, n + 1, Acquire, Relaxed)
            {
                n = e;
                continue;
            }

            return WeakToo { ptr: arc.ptr };
        }
    }
}

impl<T> Deref for ArcToo<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // Safety: Since there's an Arc to the data,
        // the data exists and may be shared.
        unsafe { &*self.data().data.get() }
    }
}

impl<T> Clone for ArcToo<T> {
    fn clone(&self) -> Self {
        if self.data().data_ref_count.fetch_add(1, Relaxed) > usize::MAX / 2 {
            std::process::abort();
        }

        ArcToo { ptr: self.ptr }
    }
}

impl<T> Drop for ArcToo<T> {
    fn drop(&mut self) {
        if self.data().data_ref_count.fetch_sub(1, Release) == 1 {
            fence(Acquire);
            // Safety: The data reference counter is zero,
            // so nothing will access the data anymore.
            unsafe {
                ManuallyDrop::drop(&mut *self.data().data.get());
            }
            // Dropping an Arc<T> now needs to decrement only one counter, except for
            // the last drop that sees that counter go from one to zero. In that case, the weak pointer
            // counter also needs to be decremented, such that it can reach zero once there are
            // no weak pointers left. We do this by simply creating a Weak<T> out of thin air and
            // immediately dropping it
            // Now that there's no `Arc<T>`s left,
            // drop the implicit weak pointer that represented all `Arc<T>`s.
            drop(WeakToo { ptr: self.ptr }); // Used to drop the extra +1 in alloc_ref_count that represents the existance of any Arc
        }
    }
}

pub struct WeakToo<T> {
    ptr: NonNull<ArcData<T>>,
}

unsafe impl<T: Sync + Send> Send for WeakToo<T> {}
unsafe impl<T: Sync + Send> Sync for WeakToo<T> {}

impl<T> WeakToo<T> {
    fn data(&self) -> &ArcData<T> {
        unsafe { self.ptr.as_ref() }
    }

    // This is wrapped in an Option beacause of the possibility that the last Arc dropped (along with its value of course)
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

            return Some(ArcToo { ptr: self.ptr });
        }
    }
}

impl<T> Clone for WeakToo<T> {
    fn clone(&self) -> Self {
        if self.data().alloc_ref_count.fetch_add(1, Relaxed) > usize::MAX / 2 {
            std::process::abort();
        }

        WeakToo { ptr: self.ptr }
    }
}

impl<T> Drop for WeakToo<T> {
    fn drop(&mut self) {
        if self.data().alloc_ref_count.fetch_sub(1, Release) == 1 {
            fence(Acquire);

            unsafe {
                drop(Box::from_raw(self.ptr.as_ptr()));
            }
        }
    }
}

pub struct MutexToo<T> {
    /// We’ll use an AtomicU32 set to zero or one, so we can use it with the atomic wait and wake functions.
    /// 0: unlocked
    /// 1: locked
    /// 2: locked, other threads waiting. This is meant for minimizing syscalls we need to make when unlocking.
    state: AtomicU32,
    value: UnsafeCell<T>,
}

unsafe impl<T> Sync for MutexToo<T> where T: Send {}

impl<T> MutexToo<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(0), // unlocked state
            value: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> MutexGuard<T> {
        if self.state.compare_exchange(0, 1, Acquire, Relaxed).is_err() {
            lock_contended(&self.state);
        }

        MutexGuard { mutex: self }
    }
}

/// A compare-and-exchange operation generally attempts to get exclusive access to the relevant
/// cache line, which can be more expensive than a simple load operation when executed repeatedly.
///
/// With that in mind, we come to the following lock_contended implementation:
fn lock_contended(state: &AtomicU32) {
    let mut spin_count = 0;

    while state.load(Relaxed) == 1 && spin_count < 100 {
        spin_count += 1;
        std::hint::spin_loop();
    }

    if state.compare_exchange(0, 1, Acquire, Relaxed).is_ok() {
        return;
    }

    while state.swap(2, Acquire) != 0 {
        wait(state, 2);
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a MutexToo<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    /// Note that after setting the state back to zero, it no longer indicates whether there are
    /// any waiting threads. The thread that’s woken up is responsible for setting the state
    /// back to 2, to make sure any other waiting threads are not forgotten. This is why the
    /// compare-and-exchange operation is not part of the while loop in our lock function.
    ///
    /// Here is also the cleaner example of why we have the aditional state.
    /// It is because with it we can completely skip the syscall possibility if there is only one Mutex.
    fn drop(&mut self) {
        if self.mutex.state.swap(0, Release) == 2 {
            wake_one(&self.mutex.state);
        }
    }
}

pub struct CondvarToo {
    counter: AtomicU32,
    // For avoiding unnecessary syscalls with notifying functions if nobody is waiting
    num_waiters: AtomicUsize,
}

impl CondvarToo {
    pub const fn new() -> Self {
        Self {
            counter: AtomicU32::new(0),
            num_waiters: AtomicUsize::new(0),
        }
    }

    pub fn notify_one(&self) {
        if self.num_waiters.load(Relaxed) > 0 {
            self.counter.fetch_add(1, Relaxed);
            wake_one(&self.counter);
        }
    }

    pub fn notify_all(&self) {
        if self.num_waiters.load(Relaxed) > 0 {
            self.counter.fetch_add(1, Relaxed);
            wake_all(&self.counter);
        }
    }

    /// The counter uses Relaxed atomics because it does NOT need synchronize access to the data.
    /// All real synchronization happens via the mutex unlock→lock happens-before chain.
    /// The counter only tracks “did something change?” to avoid spurious sleeps/wakeups.
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        self.num_waiters.fetch_add(1, Relaxed);

        let counter_value = self.counter.load(Relaxed);

        // Unlock the mutex by dropping the guard,
        // but remember the mutex so we can lock it again later.
        let mutex = guard.mutex;

        drop(guard);

        // Here between the dropping and waiting can a spurious wakeup happen.
        // This means that as soon as the Condvar::wait() method unlocks the
        // mutex, that might immediately unblock a notifying thread that was waiting for the
        // mutex. At that point the two threads are racing: the waiting thread to go to sleep, and
        // the notifying thread to lock and unlock the mutex and notify the condition variable.
        // If the notifying thread wins that race, the waiting thread will not go to sleep because
        // of the incremented counter, but the notifying thread will still call wake_one(). This is
        // exactly the problematic situation described above, where it might unnecessarily wake
        // up an extra waiting thread.

        // Wait, but only if the counter hasn't changed since unlocking.
        // There is no while loop here, it will be outside around the condition variable,
        // because not every variable has the same condition so we cannot test for that here.
        wait(&self.counter, counter_value);

        self.num_waiters.fetch_sub(1, Relaxed);

        mutex.lock()
    }
}

/// A simple example showing how to use `CondvarToo` and `MutexToo`.
///
/// # Examples
///
/// ```
/// use std::thread;
/// use NetworkTests::{CondvarToo, MutexToo};
///
/// let mutex = MutexToo::new(0);
/// let condvar = CondvarToo::new();
///
/// thread::scope(|s| {
///     // Main thread acquires the lock first, ensuring it will wait
///     let mut guard = mutex.lock();
///     
///     s.spawn(|| {
///         // This thread will have to wait for the lock
///         let mut guard = mutex.lock();
///         *guard = 42;
///         condvar.notify_one();
///     });
///     
///     // Now we wait while holding the lock
///     // The spawned thread is blocked until we release it by waiting
///     while *guard != 42 {
///         guard = condvar.wait(guard);
///     }
///     
///     assert_eq!(*guard, 42);
/// });
/// ```

// This is a lock optimized for the “frequent reading and infrequent writing" use case.
// For a more general purpose reader-writer lock, however, it is definitely worth opti‐
// mizing further, to bring the performance of write-locking and -unlocking near the
// performance of an efficient 3-state mutex.
pub struct RwLock<T> {
    /// The number of readers, or u32::MAX if write-locked.
    state: AtomicU32,
    /// Incremented on each write lock acquisition. Prevents spurious wakeups
    /// when the write lock is contended: without this, rapid reader churn could
    /// cause wait() to return immediately with a different (but still locked)
    /// state value, resulting in a busy loop instead of proper blocking.
    writer_wake_counter: AtomicU32,
    value: UnsafeCell<T>,
}

unsafe impl<T> Sync for RwLock<T> where T: Send + Sync {}

impl<T> RwLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(0),
            writer_wake_counter: AtomicU32::new(0),
            value: UnsafeCell::new(value),
        }
    }

    pub fn read(&self) -> ReadGuard<T> {
        let mut s = self.state.load(Relaxed);
        loop {
            if s % 2 == 0 {
                // Even.
                assert!(s != u32::MAX - 2, "too many readers");
                match self.state.compare_exchange_weak(s, s + 2, Acquire, Relaxed) {
                    Ok(_) => return ReadGuard { rwlock: self },
                    Err(e) => s = e,
                }
            }
            if s % 2 == 1 {
                // Odd.
                wait(&self.state, s);
                s = self.state.load(Relaxed);
            }
        }
    }

    pub fn write(&self) -> WriteGuard<T> {
        let mut s = self.state.load(Relaxed);
        loop {
            // Try to lock if unlocked.
            if s <= 1 {
                match self.state.compare_exchange(s, u32::MAX, Acquire, Relaxed) {
                    Ok(_) => return WriteGuard { rwlock: self },
                    Err(e) => {
                        s = e;
                        continue;
                    }
                }
            }

            // Block new readers, by making sure the state is odd.
            if s % 2 == 0 {
                match self.state.compare_exchange(s, s + 1, Relaxed, Relaxed) {
                    Ok(_) => {}
                    Err(e) => {
                        s = e;
                        continue;
                    }
                }
            }

            // The acquire-load operation of writer_wake_counter will form a happens-before
            // relationship with a release-increment operation that’s executed right after unlocking
            // the state, before waking up a waiting writer
            let w = self.writer_wake_counter.load(Acquire);

            s = self.state.load(Relaxed);

            if s >= 2 {
                wait(&self.writer_wake_counter, w);
                s = self.state.load(Relaxed);
            }
        }
    }
}

pub struct ReadGuard<'a, T> {
    rwlock: &'a RwLock<T>,
}

impl<T> Deref for ReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.rwlock.value.get() }
    }
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        // Decrement the state by 2 to remove one read-lock.
        if self.rwlock.state.fetch_sub(2, Release) == 3 {
            // The Release ordering on state.fetch_sub above synchronizes-with
            // the Acquire ordering on writer_wake_counter.load in write().
            // This ensures a waiting writer cannot observe the incremented counter
            // while still seeing the old (locked) state value. Without this ordering,
            // a writer could miss the wakeup and block forever despite the lock being free.

            // If we decremented from 3 to 1, that means
            // the RwLock is now unlocked _and_ there is
            // a waiting writer, which we wake up
            self.rwlock.writer_wake_counter.fetch_add(1, Release);

            wake_one(&self.rwlock.writer_wake_counter);
        }
    }
}

pub struct WriteGuard<'a, T> {
    rwlock: &'a RwLock<T>,
}

impl<T> Deref for WriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.rwlock.value.get() }
    }
}

impl<T> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.rwlock.value.get() }
    }
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        // Release ordering on state.store synchronizes-with Acquire
        // on writer_wake_counter.load in write(). This prevents writers from
        // observing the incremented counter before seeing the unlocked state.
        self.rwlock.state.store(0, Release);
        self.rwlock.writer_wake_counter.fetch_add(1, Release);
        wake_one(&self.rwlock.writer_wake_counter);
        wake_all(&self.rwlock.state);
    }
}

pub struct SemaphoreToo {
    mutex: MutexToo<usize>,
    condvar: CondvarToo,
}

impl SemaphoreToo {
    /// Creates a new semaphore with the given number of permits
    pub const fn new(permits: usize) -> Self {
        Self {
            mutex: MutexToo::new(permits),
            condvar: CondvarToo::new(),
        }
    }

    /// Acquires a permit, blocking if none are available
    pub fn acquire(&self) {
        let mut guard = self.mutex.lock();

        // Wait while no permits are available
        while *guard == 0 {
            guard = self.condvar.wait(guard);
        }

        // Take a permit
        *guard -= 1;
    }

    /// Releases a permit, waking up one waiting thread
    pub fn release(&self) {
        let mut guard = self.mutex.lock();
        *guard += 1;
        drop(guard);

        // Wake up one waiting thread
        self.condvar.notify_one();
    }

    /// Tries to acquire a permit without blocking
    /// Returns true if successful, false if no permits available
    pub fn try_acquire(&self) -> bool {
        let mut guard = self.mutex.lock();

        if *guard > 0 {
            *guard -= 1;
            true
        } else {
            false
        }
    }

    /// Returns the current number of available permits
    pub fn available_permits(&self) -> usize {
        let guard = self.mutex.lock();
        *guard
    }
}

/// A simple example showing how to use `SemaphoreToo`.
///
/// # Examples
///
/// ```
/// use std::thread;
/// use NetworkTests::SemaphoreToo;
///
/// let semaphore = SemaphoreToo::new(2); // Only 2 threads can proceed at once
///
/// thread::scope(|s| {
///     for i in 0..5 {
///         let sem = &semaphore;
///         s.spawn(move || {
///             sem.acquire();
///             println!("Thread {} acquired permit", i);
///             thread::sleep(std::time::Duration::from_millis(100));
///             println!("Thread {} releasing permit", i);
///             sem.release();
///         });
///     }
/// });
/// ```

pub struct SemaphoreAtomic {
    /// Permits available (or negative for number of waiters)
    permits: AtomicU32,
}

impl SemaphoreAtomic {
    pub const fn new(permits: u32) -> Self {
        Self {
            permits: AtomicU32::new(permits),
        }
    }

    pub fn acquire(&self) {
        let mut current = self.permits.load(Relaxed);

        loop {
            // Try fast path: if permits available, take one
            if current > 0 {
                match self
                    .permits
                    .compare_exchange_weak(current, current - 1, Acquire, Relaxed)
                {
                    Ok(_) => return,
                    Err(actual) => {
                        current = actual;
                        continue;
                    }
                }
            }

            // No permits available, wait
            wait(&self.permits, current);
            current = self.permits.load(Relaxed);
        }
    }

    pub fn release(&self) {
        self.permits.fetch_add(1, Release);
        wake_one(&self.permits);
    }

    pub fn try_acquire(&self) -> bool {
        let mut current = self.permits.load(Relaxed);

        loop {
            if current == 0 {
                return false;
            }

            match self
                .permits
                .compare_exchange_weak(current, current - 1, Acquire, Relaxed)
            {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    pub fn available_permits(&self) -> u32 {
        self.permits.load(Relaxed)
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
