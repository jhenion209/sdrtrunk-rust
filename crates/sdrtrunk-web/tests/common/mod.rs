//! Shared helpers for the web integration tests: in-process router, requests, logged-in sessions.
#![allow(dead_code)]
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use sdrtrunk_web::app::{router, AppState};
use sdrtrunk_web::auth::{Function, Role, Throttle};
use sdrtrunk_web::db::{Db, System};
use sdrtrunk_web::receiver::{Receivers, Sdrs};
use sdrtrunk_web::recorder::RecordPolicy;

pub const PW: &str = "correct horse battery";

/// System name of the one system every test state starts with.
pub const SYS: &str = "P25";

/// One enabled system ("P25", id 1) on the given SDRs, started.
pub fn state_with(sdrs: Sdrs) -> AppState {
    let db = Db::open_in_memory().unwrap();
    db.save_system(&System::default()).unwrap();
    let receivers = Receivers::new(sdrs);
    receivers.restart_all(&db);
    AppState {
        db: Arc::new(db),
        receivers: Arc::new(receivers),
        throttle: Arc::new(Throttle::default()),
        setup_code: Arc::new(Mutex::new(Some("ABCD-EFGH".into()))),
        trusted: Arc::default(),
        web: Arc::default(),
        live: Arc::default(),
    }
}

/// One fake Airspy that cannot be opened (the system shows an error, which these tests do not mind).
pub fn state() -> AppState {
    state_with(Sdrs { list: Arc::new(|| vec!["735863DC-2F67AFE3".into()]), open: Arc::new(|_, _, _| Err("test".into())) })
}

/// What system 1's recorder records (it reloads on every talkgroup change).
pub fn policy(st: &AppState) -> Arc<Mutex<RecordPolicy>> { st.receivers.get(1).expect("system 1 running").policy.clone() }

pub struct Resp { pub status: StatusCode, pub location: Option<String>, pub cookie: Option<String>, pub body: String }

pub async fn send(st: &AppState, method: &str, path: &str, cookie: Option<&str>, form: &[(&str, &str)]) -> Resp {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(c) = cookie { req = req.header(header::COOKIE, c); }
    let body = if method == "POST" {
        req = req.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        Body::from(serde_urlencoded::to_string(form).unwrap())
    } else { Body::empty() };
    send_req(st, req.body(body).unwrap()).await
}

/// Sends a request built by the test (extensions, headers) through the router.
#[allow(dead_code)]
pub async fn send_req(st: &AppState, req: Request<Body>) -> Resp {
    let res = router(st.clone()).oneshot(req).await.unwrap();
    let h = |n| res.headers().get(n).map(|v: &axum::http::HeaderValue| v.to_str().unwrap().to_string());
    let (status, location, cookie) = (res.status(), h(header::LOCATION), h(header::SET_COOKIE));
    let body = String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned();
    Resp { status, location, cookie, body }
}

/// Creates the user if needed and returns (cookie header, csrf token).
/// Admins it creates may use every function (the tests written before permissions expect that).
pub fn login_as(st: &AppState, username: &str, role: Role) -> (String, String) {
    let user = st.db.users().into_iter().find(|u| u.username == username).unwrap_or_else(|| {
        let u = st.db.create_user(username, PW, role, 0).unwrap();
        if role == Role::Admin { st.db.set_allowed(u.id, &Function::ALL).unwrap(); }
        u
    });
    let token = st.db.create_session(user.id, sdrtrunk_web::receiver::now_ms(), "local", "");
    let csrf = st.db.session(&token, sdrtrunk_web::receiver::now_ms(), "").unwrap().csrf;
    (format!("sdrtrunk_rust_session={token}"), csrf)
}

/// An admin allowed exactly `fs` (created if needed); returns (cookie header, csrf token).
pub fn login_with(st: &AppState, username: &str, fs: &[Function]) -> (String, String) {
    let (cookie, csrf) = login_as(st, username, Role::Admin);
    let id = st.db.users().into_iter().find(|u| u.username == username).unwrap().id;
    st.db.set_allowed(id, fs).unwrap();
    (cookie, csrf)
}
