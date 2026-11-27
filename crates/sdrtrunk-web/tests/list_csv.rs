//! The Talkgroups page's own CSV files: export, import with a review, and bulk delete.
mod common;
use axum::http::StatusCode;
use common::*;
use sdrtrunk_web::db::{RadioName, Talkgroup};

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
