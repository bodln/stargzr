use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use local_ip_address;
use stargzr::{player};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceBuilder;
use stargzr::middlewares::{Logger, RateLimit, SpawnRequest, echo};

type _BoxError = Box<dyn std::error::Error + Send + Sync>;

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

    //  #############
    //  #   Player  #
    //  #############

    tokio::spawn(player::initialize(PathBuf::from(
        "D:/Skola/.projekti/Tests/NetworkTests/NetworkTests/music",
    )));

    //  ####################
    //  #   Hyper testing  #
    //  ####################

    // Client
    tokio::task::spawn(async move {
        let uri_str = format!("http://{}:{}", help_ip, port);

        let uri: hyper::Uri = uri_str.parse().expect("Invalid URI");

        let host = uri.host().expect("Uri has no host");
        let port = uri.port_u16().unwrap_or(8080);

        let address = format!("{}:{}", host, port);

        println!("Client prepared for host: {}:{}", host, port);

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

    // Server
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

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C signal handler");
}