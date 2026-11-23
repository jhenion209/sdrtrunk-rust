mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use http_body_util::BodyExt;
use sdrtrunk_web::app::{router, AppState};
use sdrtrunk_web::db::System;
use tower::ServiceExt;

/// (status, content type, body) of a GET with an optional cookie.
async fn get(st: &AppState, path: &str, cookie: Option<&str>) -> (StatusCode, String, String) {
    let mut req = Request::builder().uri(path);
    if let Some(c) = cookie { req = req.header(header::COOKIE, c); }
    let res = router(st.clone()).oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = res.status();
    let ct = res.headers().get(header::CONTENT_TYPE).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    (status, ct, String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

/// Changes system 1 and restarts the systems.
fn edit(st: &AppState, f: impl FnOnce(&mut System)) {
    let mut s = st.db.systems().into_iter().next().unwrap();
    f(&mut s);
    st.db.save_system(&s).unwrap();
    st.receivers.restart_all(&st.db);
}

#[tokio::test]
async fn health_needs_no_login() {
    let st = state();
    for cookie in [None, Some("sid=stale")] {
        let (status, ct, body) = get(&st, "/health", cookie).await;
        assert!(status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE, "{status}: {body}");
        assert!(ct.starts_with("text/plain"), "{ct}");
    }
}

#[tokio::test]
async fn missing_tuner_is_503() {
    let st = state();
    edit(&st, |s| s.tuner = "0000000A-0000000B".into());
    let (status, _, body) = get(&st, "/health", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, format!("{SYS}: tuner missing\n"));
    assert!(!body.contains("0000000A-0000000B"));
}

#[tokio::test]
async fn one_system_and_unknown() {
    let st = state();
    let (_, _, body) = get(&st, &format!("/health/{SYS}"), None).await;
    assert!(body.starts_with(&format!("{SYS}: ")), "{body}");
    assert_eq!(get(&st, "/health/nope", None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn name_with_space_decodes() {
    let st = state();
    edit(&st, |s| s.system_name = "Sedgwick Co".into());
    let (status, _, body) = get(&st, "/health/Sedgwick%20Co", None).await;
    assert_ne!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.starts_with("Sedgwick Co: "), "{body}");
}

#[tokio::test]
async fn disabled_system_omitted() {
    let st = state();
    edit(&st, |s| s.enabled = false);
    let (status, _, body) = get(&st, "/health", None).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, ""));
}
