//! The server the API runs on: what is limited before a request is even read.
//!
//! `axum::serve` accepts every connection and waits as long as a client likes for its request
//! headers, so a few hundred idle or trickling connections could keep the API busy for good. Here
//! the number of connections is capped, a connection over the cap is closed at once, and a
//! client that does not finish its headers in time is cut off.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower::Service;

/// How the server limits connections.
#[derive(Debug, Clone, Copy)]
pub struct ServerLimits {
    /// Connections served at once. The API is for a few operators and tools.
    pub max_connections: usize,
    /// How long a client has to send the headers of a request (also between two requests on one
    /// connection).
    pub header_timeout: Duration,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            max_connections: 256,
            header_timeout: Duration::from_secs(10),
        }
    }
}

/// How far apart the messages about a full server are.
const LOG_EVERY: Duration = Duration::from_secs(10);

/// Serves `router` on `listener` until `shutdown` is cancelled, then lets requests under way
/// finish and returns once every connection is closed.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    limits: ServerLimits,
    shutdown: CancellationToken,
) {
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    let connections = TaskTracker::new();
    let full_log = closure_core::throttle::LogGate::new(LOG_EVERY);
    let mut protocol = http1::Builder::new();
    protocol
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_timeout);

    loop {
        let accepted = tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                // Running out of file descriptors and the like: do not spin on it.
                tracing::warn!(%error, "could not accept a connection");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            drop(stream);
            if let Some(held_back) = full_log.pass() {
                tracing::warn!(
                    held_back,
                    max = limits.max_connections,
                    "too many connections; refusing new ones"
                );
            }
            continue;
        };

        let router = router.clone();
        let protocol = protocol.clone();
        let shutdown = shutdown.clone();
        connections.spawn(async move {
            let _permit = permit;
            let service = service_fn(move |mut request: hyper::Request<Incoming>| {
                request
                    .extensions_mut()
                    .insert(ConnectInfo::<SocketAddr>(peer));
                router.clone().call(request)
            });
            let connection = protocol.serve_connection(TokioIo::new(stream), service);
            tokio::pin!(connection);
            let result = tokio::select! {
                result = connection.as_mut() => result,
                () = shutdown.cancelled() => {
                    connection.as_mut().graceful_shutdown();
                    connection.await
                }
            };
            if let Err(error) = result {
                // Clients that leave or are too slow are routine.
                tracing::debug!(%peer, %error, "connection ended with an error");
            }
        });
    }
    connections.close();
    connections.wait().await;
}

#[cfg(test)]
mod tests {
    use axum::routing::get;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::*;

    struct Running {
        address: SocketAddr,
        shutdown: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }

    async fn start(limits: ServerLimits) -> Running {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let router = Router::new().route("/", get(|| async { "ok" }));
        let task = tokio::spawn(serve(listener, router, limits, shutdown.clone()));
        Running {
            address,
            shutdown,
            task,
        }
    }

    /// Sends one request and returns what came back, or `None` if the connection was closed
    /// without an answer.
    async fn get_root(address: SocketAddr) -> Option<String> {
        let mut stream = TcpStream::connect(address).await.ok()?;
        stream
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .await
            .ok()?;
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.ok()?;
        (!answer.is_empty()).then_some(answer)
    }

    #[tokio::test]
    async fn it_serves_requests_and_stops_on_shutdown() {
        let server = start(ServerLimits::default()).await;
        let answer = get_root(server.address).await.expect("an answer");
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");

        server.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), server.task)
            .await
            .expect("the server stops")
            .unwrap();
    }

    #[tokio::test]
    async fn a_client_that_does_not_finish_its_headers_is_cut_off() {
        let server = start(ServerLimits {
            max_connections: 8,
            header_timeout: Duration::from_millis(200),
        })
        .await;
        let mut stream = TcpStream::connect(server.address).await.unwrap();
        // The request line and one header; the blank line that ends them never comes.
        stream
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\n")
            .await
            .unwrap();

        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut rest))
            .await
            .expect("the server hangs up")
            .ok();
        assert!(!String::from_utf8_lossy(&rest).contains("200 OK"));
    }

    #[tokio::test]
    async fn connections_over_the_limit_are_closed_at_once_and_room_comes_back() {
        let server = start(ServerLimits {
            max_connections: 2,
            header_timeout: Duration::from_secs(30),
        })
        .await;
        // Two connections that say nothing hold the two places.
        let first = TcpStream::connect(server.address).await.unwrap();
        let second = TcpStream::connect(server.address).await.unwrap();
        // The server has taken both once it answers a third by closing it.
        let mut third = TcpStream::connect(server.address).await.unwrap();
        let mut buffer = [0_u8; 16];
        let read = tokio::time::timeout(Duration::from_secs(5), third.read(&mut buffer))
            .await
            .expect("the third connection is closed at once");
        assert!(matches!(read, Ok(0) | Err(_)));

        drop(first);
        // Room is given back when the connection is seen to be gone.
        let mut answered = false;
        for _ in 0..50 {
            if get_root(server.address).await.is_some() {
                answered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(answered, "a freed place is used again");
        drop(second);
    }
}
