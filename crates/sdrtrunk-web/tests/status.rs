mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use http_body_util::BodyExt;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use sdrtrunk_web::app::router;
use sdrtrunk_web::auth::Role;
use sdrtrunk_web::db::Talkgroup;
use sdrtrunk_web::receiver::{CallEvent, Sdrs};

fn call(group: u16) -> CallEvent {
    CallEvent { system: "P25".into(), alias_list: "P25".into(), at_ms: 1, group, source: 733351, hz: 859_737_500, encrypted: true, emergency: false }
}

/// Opens /events and returns the body for incremental reading.
async fn open_events(st: &sdrtrunk_web::app::AppState, cookie: &str) -> Body {
    let req = Request::get("/events").header(header::COOKIE, cookie).body(Body::empty()).unwrap();
    let res = router(st.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    res.into_body()
}

/// Reads SSE frames until one contains `needle` (or gives up after 3 s).
async fn read_until(body: &mut Body, needle: &str) -> String {
    let mut seen = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(frame)) = body.frame().await {
            if let Some(d) = frame.data_ref() { seen.push_str(&String::from_utf8_lossy(d)); }
            if seen.contains(needle) { break; }
        }
    }).await;
    seen
}

#[tokio::test]
async fn status_shows_error() {
    let st = state_with(Sdrs { list: Arc::new(|| vec!["735863DC-2F67AFE3".into()]),
        open: Arc::new(|_, _, _| Err("airspy_open failed with libairspy code -5".into())) });
    for _ in 0..100 {
        if st.receivers.status(1).unwrap().state == "error" { break; }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (cookie, _) = login_as(&st, "owner", Role::Owner);
    let r = send(&st, "GET", "/", Some(&cookie), &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("airspy_open failed with libairspy code -5"), "{}", r.body);
    assert!(r.body.contains("858.9625"), "control channel shown");
    assert!(r.body.contains("P25 / Control"), "one box per system, by name");
}

#[tokio::test]
async fn events_stream_calls() {
    let st = state();
    st.db.upsert_talkgroup(SYS, &Talkgroup { number: 85, name: "Fire".into(), record: true }).unwrap();
    let (cookie, _) = login_as(&st, "owner", Role::Owner);
    let mut body = open_events(&st, &cookie).await;
    let status = read_until(&mut body, "event: status").await;
    assert!(status.contains("event: status"), "{status}");
    st.receivers.calls.send(call(85)).unwrap();
    let seen = read_until(&mut body, "event: call").await;
    assert!(seen.contains("event: call") && seen.contains("\"name\":\"Fire\"") && seen.contains("\"group\":85") && seen.contains("\"system\":\"P25\""), "{seen}");
}

#[tokio::test]
async fn events_survive_lag() {
    let st = state();
    let (cookie, _) = login_as(&st, "owner", Role::Owner);
    let mut body = open_events(&st, &cookie).await;
    for _ in 0..300 { st.receivers.calls.send(call(1)).unwrap(); }
    st.receivers.calls.send(call(999)).unwrap();
    let seen = read_until(&mut body, "\"group\":999").await;
    assert!(seen.contains("\"group\":999"), "stream closed or stuck after lag");
}

#[tokio::test]
async fn events_end_when_session_revoked() {
    let st = state();
    login_as(&st, "owner", Role::Owner);
    let (cookie, _) = login_as(&st, "admin", Role::Admin);
    let mut body = open_events(&st, &cookie).await;
    read_until(&mut body, "event: status").await;
    let id = st.db.users().into_iter().find(|u| u.username == "admin").unwrap().id;
    st.db.remove_user(id).unwrap();
    st.receivers.calls.send(call(5)).unwrap();
    let seen = read_until(&mut body, "\"group\":5").await;
    assert!(!seen.contains("\"group\":5"), "removed admin still receives calls: {seen}");
}

#[tokio::test]
async fn long_username_not_tracked() {
    let st = state();
    let name = "x".repeat(5000);
    let r = send(&st, "POST", "/login", None, &[("username", name.as_str()), ("password", "whatever password")]).await;
    assert!(r.body.contains("Wrong username or password"));
    assert_eq!(st.throttle.tracked(), 1, "only the address key is kept, not a 5000-byte username");
}

#[tokio::test]
async fn events_need_login() {
    let st = state();
    let r = send(&st, "GET", "/events", None, &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
}
