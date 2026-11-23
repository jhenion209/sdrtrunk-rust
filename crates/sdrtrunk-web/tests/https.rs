//! HTTPS by default: the program's own certificate, made once; the owner's pair, checked; plain HTTP only when set;
//! both stop promptly with a live page open.
mod common;
use common::*;
use sdrtrunk_web::acme::Certs;
use sdrtrunk_web::app::{own_certificate, router, serve, serve_https};
use sdrtrunk_web::auth::Role;
use sdrtrunk_web::web_config::WebConfig;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn config(pairs: &[(&str, &str)]) -> WebConfig {
    WebConfig::read(|name| pairs.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string())).unwrap()
}

fn free_port() -> u16 { std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port() }

fn listener() -> (std::net::TcpListener, u16) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let port = l.local_addr().unwrap().port();
    (l, port)
}

fn client_trusting(cert: &std::path::Path) -> reqwest::Client {
    let ca = reqwest::Certificate::from_pem(&std::fs::read(cert).unwrap()).unwrap();
    reqwest::Client::builder().add_root_certificate(ca).timeout(Duration::from_secs(5)).build().unwrap()
}

/// GETs `url` until it answers (the server may still be starting); the status.
async fn answer(client: &reqwest::Client, url: &str) -> u16 {
    for _ in 0..50 {
        if let Ok(r) = client.get(url).send().await { return r.status().as_u16(); }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{url} never answered");
}

#[tokio::test]
async fn health_answers_over_https_with_the_own_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let certs = Arc::new(Certs::new(Arc::new(own_certificate(dir.path()).unwrap())));
    let (l, port) = listener();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve_https(l, router(state()), certs, async { let _ = stopped.await; }, Duration::from_millis(300)));
    let client = client_trusting(&dir.path().join("tls/cert.pem"));
    assert_eq!(answer(&client, &format!("https://localhost:{port}/login")).await, 200, "the name in the certificate");
    assert_eq!(answer(&client, &format!("https://127.0.0.1:{port}/login")).await, 200, "the address in the certificate");
    assert!(reqwest::get(format!("http://127.0.0.1:{port}/login")).await.is_err(), "no plain HTTP on the HTTPS port");
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap().unwrap();
}

#[test]
fn own_certificate_is_made_once() {
    let dir = tempfile::tempdir().unwrap();
    own_certificate(dir.path()).unwrap();
    let (cert, key) = (dir.path().join("tls/cert.pem"), dir.path().join("tls/key.pem"));
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&cert), 0o644);
    assert_eq!(mode(&key), 0o600, "the key is the service account's alone");
    let before = (std::fs::read(&cert).unwrap(), std::fs::read(&key).unwrap());
    own_certificate(dir.path()).unwrap();
    assert_eq!(before, (std::fs::read(&cert).unwrap(), std::fs::read(&key).unwrap()), "a restart keeps the certificate");
}

#[tokio::test]
async fn an_open_event_stream_does_not_block_https_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let certs = Arc::new(Certs::new(Arc::new(own_certificate(dir.path()).unwrap())));
    let st = state();
    let (cookie, _) = login_as(&st, "admin", Role::Admin);
    let (l, port) = listener();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve_https(l, router(st), certs, async { let _ = stopped.await; }, Duration::from_millis(300)));
    let client = client_trusting(&dir.path().join("tls/cert.pem"));
    answer(&client, &format!("https://localhost:{port}/login")).await;
    let events = client.get(format!("https://localhost:{port}/events")).header("cookie", cookie).send().await.unwrap();
    assert!(events.headers()["content-type"].to_str().unwrap().contains("text/event-stream"));
    let at = Instant::now();
    stop.send(()).unwrap();
    let done = tokio::time::timeout(Duration::from_secs(5), server).await;
    assert!(done.is_ok(), "serving did not end with a stream open");
    assert!(at.elapsed() < Duration::from_secs(2), "{:?}", at.elapsed());
    drop(events);
}

/// `serve` on a fresh database in `dir`, stopped by the returned sender.
fn start(dir: &std::path::Path, web: WebConfig) -> (tokio::sync::oneshot::Sender<()>, tokio::task::JoinHandle<std::io::Result<()>>) {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let sdrs = sdrtrunk_web::receiver::Sdrs { list: Arc::new(Vec::new), open: Arc::new(|_, _, _| Err("test".into())) };
    let task = tokio::spawn(serve(dir.join("t.db"), web, sdrs, async { let _ = stopped.await; }));
    (stop, task)
}

#[tokio::test]
async fn https_and_plain_http_serve_together_and_stop_together() {
    let dir = tempfile::tempdir().unwrap();
    let (s, h) = (free_port(), free_port());
    let (stop, task) = start(dir.path(), config(&[("SDRTRUNK_HTTPS", &format!("127.0.0.1:{s}")), ("SDRTRUNK_HTTP", &format!("127.0.0.1:{h}"))]));
    let plain = reqwest::Client::new();
    assert_eq!(answer(&plain, &format!("http://127.0.0.1:{h}/login")).await, 200);
    let client = client_trusting(&dir.path().join("tls/cert.pem"));
    assert_eq!(answer(&client, &format!("https://127.0.0.1:{s}/login")).await, 200);
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn plain_http_alone_when_https_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let h = free_port();
    let (stop, task) = start(dir.path(), config(&[("SDRTRUNK_HTTPS", "off"), ("SDRTRUNK_HTTP", &format!("127.0.0.1:{h}"))]));
    assert_eq!(answer(&reqwest::Client::new(), &format!("http://127.0.0.1:{h}/login")).await, 200);
    assert!(!dir.path().join("tls").exists(), "no certificate is made without HTTPS");
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn a_held_port_is_refused_with_its_address() {
    let dir = tempfile::tempdir().unwrap();
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let (_stop, task) = start(dir.path(), config(&[("SDRTRUNK_HTTPS", &format!("127.0.0.1:{port}"))]));
    let e = tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap().unwrap_err().to_string();
    assert!(e.starts_with(&format!("cannot listen on 127.0.0.1:{port}:")), "{e}");
}

#[tokio::test]
async fn owners_pair_problems_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nowhere/cert.pem");
    let (_stop, task) = start(dir.path(), config(&[("SDRTRUNK_HTTPS", &format!("127.0.0.1:{}", free_port())),
        ("SDRTRUNK_TLS_CERT", missing.to_str().unwrap()), ("SDRTRUNK_TLS_KEY", "/nowhere/key.pem")]));
    let e = tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap().unwrap_err().to_string();
    assert!(e.contains(missing.to_str().unwrap()), "{e}");
    // A certificate with another pair's key.
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    own_certificate(&a).unwrap();
    own_certificate(&b).unwrap();
    let (_stop, task) = start(dir.path(), config(&[("SDRTRUNK_HTTPS", &format!("127.0.0.1:{}", free_port())),
        ("SDRTRUNK_TLS_CERT", a.join("tls/cert.pem").to_str().unwrap()), ("SDRTRUNK_TLS_KEY", b.join("tls/key.pem").to_str().unwrap())]));
    let e = tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap().unwrap_err().to_string();
    assert!(e.contains("is not the key of"), "{e}");
    assert!(!e.contains("PRIVATE KEY"), "{e}");
}
