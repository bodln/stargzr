use http_body_util::{BodyExt, combinators::BoxBody};
use hyper::body::Frame;
use hyper::body::{Body, Bytes};
use hyper::server::conn::http1;
use hyper::{Method, StatusCode};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use local_ip_address;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceBuilder;

use network_tests::middlewares::{Logger, RateLimit, SpawnRequest, full, empty};

type _BoxError = Box<dyn std::error::Error + Send + Sync>;

// Main request handler - routes incoming HTTP requests
// Takes: Request with incoming body stream
// Returns: Response with a BoxBody (type-erased body that streams Bytes chunks)
async fn echo(
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

        // Uncomment for testing the middlewares

        // let output = Command::new("hey")
        //     .args(["-z", "2s", "-c", "3", "http://192.168.1.2:7878/"])
        //     .output().await
        //     .expect("failed to execute hey");

        // let stdout = String::from_utf8_lossy(&output.stdout);
        // let stderr = String::from_utf8_lossy(&output.stderr);

        // println!("--- hey output ---\n{stdout}");
        // if !stderr.is_empty() {
        //     eprintln!("--- hey errors ---\n{stderr}");
        // }

        let _stream = TcpStream::connect(address).await;
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
                    let requests_per_delay = 1;

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
