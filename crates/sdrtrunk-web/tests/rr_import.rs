mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use http_body_util::BodyExt;
use tower::ServiceExt;
use sdrtrunk_web::app::{router, AppState};
use sdrtrunk_web::auth::Role;
use sdrtrunk_web::db::Talkgroup;

const CSV: &str = "Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category\r\n\
1,001,\"Wichita PD West\",\"D\",\"West Dispatch\",\"Law Dispatch\",\"Wichita Police\"\r\n\
3,003,\"Same\",\"D\",\"x\",\"x\",\"x\"\r\n\
5,005,\"North\",\"D\",\"x\",\"x\",\"x\"\r\n\
85,055,\"Fire Tac\",\"DE\",\"x\",\"x\",\"County Fire\"\r\n";

async fn upload(st: &AppState, cookie: &str, csrf: &str, csv: &str) -> (StatusCode, Option<String>, String) {
    upload_into(st, cookie, csrf, csv, "P25", "").await
}

async fn upload_into(st: &AppState, cookie: &str, csrf: &str, csv: &str, into: &str, new_name: &str) -> (StatusCode, Option<String>, String) {
    let b = "XbOuNdArY";
    let body = format!("--{b}\r\nContent-Disposition: form-data; name=\"csrf\"\r\n\r\n{csrf}\r\n\
--{b}\r\nContent-Disposition: form-data; name=\"into\"\r\n\r\n{into}\r\n\
--{b}\r\nContent-Disposition: form-data; name=\"new_name\"\r\n\r\n{new_name}\r\n\
--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"rr.csv\"\r\nContent-Type: text/csv\r\n\r\n{csv}\r\n--{b}--\r\n");
    let req = Request::builder().method("POST").uri("/talkgroups/import").header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={b}")).body(Body::from(body)).unwrap();
    let res = router(st.clone()).oneshot(req).await.unwrap();
    let (status, loc) = (res.status(), res.headers().get(header::LOCATION).map(|v| v.to_str().unwrap().to_string()));
    (status, loc, String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

fn tg(n: u16, name: &str, record: bool) -> Talkgroup { Talkgroup { number: n, name: name.into(), record } }

#[tokio::test]
async fn import_adds_new_and_asks_about_conflicts() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    st.db.upsert_talkgroup(SYS, &tg(1, "SG WPD West", false)).unwrap();
    st.db.upsert_talkgroup(SYS, &tg(3, "Same", true)).unwrap();
    let (status, _, page) = upload(&st, &cookie, &csrf, CSV).await;
    assert_eq!(status, StatusCode::OK, "conflicts shown for approval");
    assert!(page.contains("SG WPD West") && page.contains("Wichita PD West") && page.contains("Keep: "));
    assert!(page.contains("2 new talkgroups added"), "{page}");
    let t = st.db.talkgroups(SYS);
    assert_eq!(t.iter().find(|t| t.number == 1).unwrap(), &tg(1, "SG WPD West", false), "conflict not overwritten yet");
    assert_eq!(t.iter().find(|t| t.number == 85).unwrap(), &tg(85, "Fire Tac", false), "encrypted: record off");
    assert!(t.iter().find(|t| t.number == 5).unwrap().record);
    assert_eq!(st.db.new_talkgroups(SYS), vec![5, 85]);
    assert_eq!(policy(&st).lock().unwrap().talkgroups.len(), 4, "recorder sees the import");

    let c = Some(cookie.as_str());
    let form = [("csrf", csrf.as_str()), ("list", SYS), ("conflict", "1"), ("rr_name_1", "Wichita PD West"), ("choice_1", "rr")];
    let r = send(&st, "POST", "/talkgroups/import/apply", c, &form).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(st.db.talkgroups(SYS)[0], tg(1, "Wichita PD West", false), "renamed, record flag kept");
    assert_eq!(st.db.audit_log(1)[0].action, "renamed 1 talkgroup on P25 from RadioReference");

    let list = send(&st, "GET", "/talkgroups", c, &[]).await.body;
    let (north, west) = (list.find("North").unwrap(), list.find("Wichita PD West").unwrap());
    assert!(north < west && list.contains("NEW"), "new talkgroups pinned to the top");
    send(&st, "POST", "/talkgroups/5/reviewed", c, &[("csrf", &csrf), ("list", SYS)]).await;
    assert_eq!(st.db.new_talkgroups(SYS), vec![85]);
    send(&st, "POST", "/talkgroups/reviewed", c, &[("csrf", &csrf), ("list", SYS)]).await;
    assert!(st.db.new_talkgroups(SYS).is_empty());
}

#[tokio::test]
async fn keep_local_and_validation() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    let c = Some(cookie.as_str());
    st.db.upsert_talkgroup(SYS, &tg(1, "Mine", true)).unwrap();
    let form = [("csrf", csrf.as_str()), ("list", SYS), ("conflict", "1"), ("rr_name_1", "Theirs"), ("choice_1", "local")];
    send(&st, "POST", "/talkgroups/import/apply", c, &form).await;
    assert_eq!(st.db.talkgroups(SYS)[0].name, "Mine", "keep local leaves the name");
    let long = "x".repeat(65);
    let bad = [("csrf", csrf.as_str()), ("list", SYS), ("conflict", "1"), ("rr_name_1", long.as_str()), ("choice_1", "rr")];
    assert_eq!(send(&st, "POST", "/talkgroups/import/apply", c, &bad).await.status, StatusCode::BAD_REQUEST);
    let unknown = [("csrf", csrf.as_str()), ("list", SYS), ("conflict", "9"), ("rr_name_9", "Ghost"), ("choice_9", "rr")];
    assert_eq!(send(&st, "POST", "/talkgroups/import/apply", c, &unknown).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(st.db.talkgroups(SYS).len(), 1, "apply never adds talkgroups");
    assert_eq!(send(&st, "POST", "/talkgroups/import/apply", c, &[("csrf", "x"), ("list", SYS)]).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn no_conflicts_redirects_and_bad_files_refused() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    let (status, loc, _) = upload(&st, &cookie, &csrf, CSV).await;
    assert_eq!((status, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/talkgroups?list=P25&imported=4")));
    assert_eq!(st.db.audit_log(1)[0].action, "imported 4 new talkgroups into P25 from RadioReference, 0 conflicts");
    let (status, _, page) = upload(&st, &cookie, &csrf, "P25, 7, \"dsd\"\n").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("not a RadioReference talkgroup CSV"));
    assert_eq!(st.db.talkgroups(SYS).len(), 4);
    assert_eq!(upload(&st, &cookie, "wrong", CSV).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn import_into_new_alias_list() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    st.db.upsert_talkgroup(SYS, &tg(1, "Mine", true)).unwrap();
    let (status, loc, _) = upload_into(&st, &cookie, &csrf, CSV, "+new", "KSICS").await;
    assert_eq!((status, loc.as_deref()), (StatusCode::SEE_OTHER, Some("/talkgroups?list=KSICS&imported=4&created=1")));
    assert_eq!(st.db.alias_lists(), vec!["KSICS", "P25"]);
    assert_eq!(st.db.talkgroups("KSICS").len(), 4, "everything added, no conflicts with another list");
    assert_eq!(st.db.talkgroups(SYS), vec![tg(1, "Mine", true)], "the current list is untouched");
    assert_eq!(st.db.audit_log(1)[0].action, "imported 4 new talkgroups into new alias list KSICS from RadioReference, 0 conflicts");
    let page = send(&st, "GET", "/talkgroups?list=KSICS&imported=4&created=1", Some(&cookie), &[]).await.body;
    assert!(page.contains("New alias list KSICS created") && page.contains("No system uses this list yet"), "{page}");
    let (status, _, page) = upload_into(&st, &cookie, &csrf, CSV, "+new", "ksics").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("already exists"), "names are unique, ignoring case");
    let (_, _, page) = upload_into(&st, &cookie, &csrf, CSV, "+new", " ").await;
    assert!(page.contains("Give the new alias list a name"));
    assert_eq!(upload_into(&st, &cookie, &csrf, CSV, "Nope", "").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(st.db.alias_lists().len(), 2, "nothing else created");
}
