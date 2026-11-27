//! The Talkgroups page's own CSV files: export, import with a review, and bulk delete.
mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use http_body_util::BodyExt;
use sdrtrunk_web::app::{router, AppState};
use sdrtrunk_web::auth::Role;
use sdrtrunk_web::db::{RadioName, Talkgroup};
use sdrtrunk_web::pages::talkgroups::NEW_LIST;
use tower::ServiceExt;

/// Posts a file to the talkgroup import (as in `tests/rr_import.rs`).
async fn upload_into(st: &AppState, cookie: &str, csrf: &str, csv: &str, into: &str, new_name: &str) -> (StatusCode, Option<String>, String) {
    let b = "XbOuNdArY";
    let body = format!("--{b}\r\nContent-Disposition: form-data; name=\"csrf\"\r\n\r\n{csrf}\r\n\
--{b}\r\nContent-Disposition: form-data; name=\"into\"\r\n\r\n{into}\r\n\
--{b}\r\nContent-Disposition: form-data; name=\"new_name\"\r\n\r\n{new_name}\r\n\
--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"list.csv\"\r\nContent-Type: text/csv\r\n\r\n{csv}\r\n--{b}--\r\n");
    let req = Request::builder().method("POST").uri("/talkgroups/import").header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={b}")).body(Body::from(body)).unwrap();
    let res = router(st.clone()).oneshot(req).await.unwrap();
    let (status, loc) = (res.status(), res.headers().get(header::LOCATION).map(|v| v.to_str().unwrap().to_string()));
    (status, loc, String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

#[tokio::test]
async fn export_both_sections() {
    let st = state();
    let (cookie, _) = login_with(&st, "viewer", &[]);
    st.db.upsert_talkgroup(SYS, &Talkgroup { number: 85, name: "Fire, Tac".into(), record: false }).unwrap();
    st.db.save_radio_name(SYS, &RadioName { radio: 7, name: "Engine 7".into() }).unwrap();
    let r = send(&st, "GET", "/talkgroups/export.csv?list=P25", Some(&cookie), &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, "number,name,record\n85,\"Fire, Tac\",no\n");
    assert!(r.headers.get("content-disposition").unwrap().to_str().unwrap().starts_with("attachment; filename=\"P25-talkgroups-"));
    assert!(r.headers.get("content-type").unwrap().to_str().unwrap().starts_with("text/csv"));
    let r = send(&st, "GET", "/talkgroups/radios/export.csv?list=P25", Some(&cookie), &[]).await;
    assert_eq!(r.body, "radio,name\n7,Engine 7\n");
    assert_eq!(send(&st, "GET", "/talkgroups/export.csv?list=nope", Some(&cookie), &[]).await.status, StatusCode::NOT_FOUND);
    assert_ne!(send(&st, "GET", "/talkgroups/export.csv?list=P25", None, &[]).await.status, StatusCode::OK);
    let page = send(&st, "GET", "/talkgroups", Some(&cookie), &[]).await;
    assert!(page.body.contains("/talkgroups/export.csv?list=P25") && page.body.contains("/talkgroups/radios/export.csv?list=P25"));
}

#[tokio::test]
async fn import_own_file_adds_then_reviews() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    st.db.upsert_talkgroup(SYS, &Talkgroup { number: 1, name: "Old".into(), record: true }).unwrap();
    st.db.upsert_talkgroup(SYS, &Talkgroup { number: 2, name: "Same".into(), record: true }).unwrap();
    let csv = "number,name,record\n1,New,no\n2,Same,yes\n3,Added,no\n";
    let (status, _, body) = upload_into(&st, &cookie, &csrf, csv, SYS, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Use file") && body.contains("/talkgroups/import/file/apply"));
    let tgs = st.db.talkgroups(SYS);
    assert_eq!(tgs[2], Talkgroup { number: 3, name: "Added".into(), record: false });
    assert_eq!(st.db.new_talkgroups(SYS), vec![3]);
    assert_eq!(tgs[0].name, "Old", "nothing changed before the choice");
    let r = send(&st, "POST", "/talkgroups/import/file/apply", Some(&cookie), &[("csrf", &csrf), ("list", SYS),
        ("conflict", "1"), ("file_name_1", "New"), ("file_record_1", "no"), ("choice_1", "file")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(st.db.talkgroups(SYS)[0], Talkgroup { number: 1, name: "New".into(), record: false });
    assert_eq!(st.db.audit_log(1)[0].action, "updated 1 talkgroup on P25 from a talkgroup CSV");
    assert_eq!(send(&st, "POST", "/talkgroups/import/file/apply", Some(&cookie), &[("csrf", &csrf), ("list", SYS),
        ("conflict", "9"), ("file_name_9", "X"), ("file_record_9", "yes"), ("choice_9", "file")]).await.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn exported_file_imports_unchanged_into_another_list() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    st.db.upsert_talkgroup(SYS, &Talkgroup { number: 85, name: "Fire, \"Tac\" Ü".into(), record: false }).unwrap();
    let csv = send(&st, "GET", "/talkgroups/export.csv?list=P25", Some(&cookie), &[]).await.body;
    let (status, loc, _) = upload_into(&st, &cookie, &csrf, &csv, NEW_LIST, "Copy").await;
    assert_eq!(status, StatusCode::SEE_OTHER, "no conflicts, straight back to the list: {loc:?}");
    assert_eq!(st.db.talkgroups("Copy"), st.db.talkgroups(SYS));
}
