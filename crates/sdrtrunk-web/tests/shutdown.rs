mod common;
use common::*;
use sdrtrunk_web::app::{router, serve_until};
use sdrtrunk_web::auth::Role;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// An open dashboard (a Server-Sent Events stream that never ends) must not keep the server from stopping: after the
/// stop signal, connections get `grace` to close, then serving ends anyway so the systems can be stopped.
#[tokio::test]
async fn an_open_event_stream_does_not_block_shutdown() {
    let st = state();
    let (cookie, _) = login_as(&st, "admin", Role::Admin);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve_until(listener, router(st), async { let _ = stopped.await; }, Duration::from_millis(300)));
    // The dashboard's live stream, held open.
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(format!("GET /events HTTP/1.1\r\nHost: x\r\nCookie: {cookie}\r\n\r\n").as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = sock.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).contains("text/event-stream"), "{}", String::from_utf8_lossy(&buf[..n]));
    let at = Instant::now();
    stop.send(()).unwrap();
    let done = tokio::time::timeout(Duration::from_secs(5), server).await;
    assert!(done.is_ok(), "serving did not end with a stream open");
    assert!(at.elapsed() < Duration::from_secs(2), "{:?}", at.elapsed());
    drop(sock);
}
