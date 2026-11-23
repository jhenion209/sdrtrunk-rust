mod common;
use axum::http::StatusCode;
use chrono::{Local, TimeZone};
use common::*;
use sdrtrunk_p25::trunk::CallGrant;
use sdrtrunk_p25::tsbk_msg::Channel;
use sdrtrunk_web::auth::{Function, Role};
use sdrtrunk_web::db::{RadioName, Settings};
use sdrtrunk_web::event_log::EventLog;
use std::sync::Arc;

fn rn(radio: u32, name: &str) -> RadioName { RadioName { radio, name: name.into() } }

#[tokio::test]
async fn owner_adds_renames_removes_radio_name() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    let c = Some(cookie.as_str());
    let r = send(&st, "POST", "/talkgroups/radios", c, &[("csrf", &csrf), ("list", SYS), ("radio", "733730"), ("name", " K315 ")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(st.db.radio_names(SYS), vec![rn(733_730, "K315")]);
    assert_eq!(policy(&st).lock().unwrap().radios.get(&733_730).map(String::as_str), Some("K315"), "recorder sees it without a restart");
    assert_eq!(st.receivers.radio_name(SYS, 733_730).as_deref(), Some("K315"), "the dashboard feed reads it from memory");
    assert_eq!(st.receivers.radio_name("Other list", 733_730), None);
    assert_eq!(st.db.audit_log(1)[0].action, "radio name 733730 in P25: K315");
    assert!(send(&st, "GET", "/talkgroups", c, &[]).await.body.contains("K315"));
    send(&st, "POST", "/talkgroups/radios", c, &[("csrf", &csrf), ("list", SYS), ("radio", "733730"), ("name", "K316")]).await;
    assert_eq!(st.db.radio_names(SYS), vec![rn(733_730, "K316")]);
    let r = send(&st, "POST", "/talkgroups/radios/733730/delete", c, &[("csrf", &csrf), ("list", SYS)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(st.db.radio_names(SYS).is_empty());
    assert!(policy(&st).lock().unwrap().radios.is_empty());
    assert_eq!(st.db.audit_log(1)[0].action, "removed radio name 733730 from P25");
    assert!(!send(&st, "GET", "/talkgroups", c, &[]).await.body.contains("K316"));
}

#[tokio::test]
async fn radio_name_rejects_bad_input() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    let c = Some(cookie.as_str());
    let long = "x".repeat(65);
    for (radio, name) in [("0", "A"), ("16777216", "A"), ("abc", "A"), ("5", " "), ("5", long.as_str())] {
        let r = send(&st, "POST", "/talkgroups/radios", c, &[("csrf", &csrf), ("list", SYS), ("radio", radio), ("name", name)]).await;
        assert_eq!(r.status, StatusCode::OK, "{radio} {name}: refused on the page");
        assert!(r.body.contains("class=\"error\""), "{radio} {name}: {}", r.body);
    }
    assert!(st.db.radio_names(SYS).is_empty());
    send(&st, "POST", "/talkgroups/radios", c, &[("csrf", &csrf), ("list", SYS), ("radio", "5"), ("name", "Car\n12\0")]).await;
    assert_eq!(st.db.radio_names(SYS), vec![rn(5, "Car 12")], "control characters become spaces");
    st.db.delete_radio_name(SYS, 5).unwrap();
    let r = send(&st, "POST", "/talkgroups/radios", c, &[("csrf", &csrf), ("list", "nope"), ("radio", "5"), ("name", "A")]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "unknown list");
    let r = send(&st, "POST", "/talkgroups/radios", c, &[("csrf", "x"), ("list", SYS), ("radio", "5"), ("name", "A")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "bad csrf");
    assert!(st.db.radio_names(SYS).is_empty());
}

#[tokio::test]
async fn admin_without_talkgroups_edit_is_refused() {
    let st = state();
    let (cookie, csrf) = login_with(&st, "helper", &[Function::TalkgroupsImport]);
    let c = Some(cookie.as_str());
    let r = send(&st, "POST", "/talkgroups/radios", c, &[("csrf", &csrf), ("list", SYS), ("radio", "5"), ("name", "A")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    st.db.save_radio_name(SYS, &rn(5, "A")).unwrap();
    let r = send(&st, "POST", "/talkgroups/radios/5/delete", c, &[("csrf", &csrf), ("list", SYS)]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(st.db.radio_names(SYS), vec![rn(5, "A")]);
}

#[tokio::test]
async fn event_log_page_shows_radio_name() {
    let st = state();
    let d = std::env::temp_dir().join(format!("sdrtrunk-radio-names-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let t = Local.with_ymd_and_hms(2026, 11, 20, 10, 0, 0).single().unwrap().timestamp_millis();
    let mut l = EventLog::new(Some(d.clone()), SYS, Arc::default(), 1, 3650);
    l.grant(&CallGrant { group: 27, source: 733_730, channel: Channel { band: 0, number: 4054 }, hz: 859_737_500, encrypted: false, emergency: false }, t);
    l.close_all(t);
    st.db.save_settings(&Settings { event_log_dir: d.display().to_string(), event_log_days: 3650, ..Settings::default() }).unwrap();
    st.db.save_radio_name(SYS, &rn(733_730, "K315")).unwrap();
    let (cookie, _) = login_as(&st, "admin", Role::Admin);
    let r = send(&st, "GET", "/event-log?from=2026-11-20T00%3A00&to=2026-11-20T23%3A59", Some(&cookie), &[]).await;
    assert!(r.body.contains("K315 (733730)"), "{}", r.body);
}

/// Playlist lists in file order: "P25 Sim" (733730 "K315", 1 "New", a range) and "Other" (9 "Elsewhere", 1 "Different").
const PLAYLIST: &str = r#"<playlist>
  <alias name="K315" list="P25 Sim"><id type="radio" value="733730" protocol="APCO25"/>
    <id type="radioRange" min="1" max="9" protocol="APCO25"/></alias>
  <alias name="New" list="P25 Sim"><id type="radio" value="1" protocol="APCO25"/></alias>
  <alias name="Elsewhere" list="Other"><id type="radio" value="9" protocol="APCO25"/></alias>
  <alias name="Different" list="Other"><id type="radio" value="1" protocol="APCO25"/></alias>
</playlist>"#;

/// POSTs the import form: `fields` as multipart text fields, plus the playlist as the file upload when `file` is set.
async fn upload(st: &sdrtrunk_web::app::AppState, cookie: &str, fields: &[(&str, &str)], file: Option<&str>) -> (StatusCode, String) {
    use axum::body::Body;
    use axum::http::{header, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let b = "XbOuNdArY";
    let mut body = String::new();
    for (k, v) in fields { body += &format!("--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"); }
    if let Some(x) = file { body += &format!("--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"default.xml\"\r\nContent-Type: text/xml\r\n\r\n{x}\r\n"); }
    body += &format!("--{b}--\r\n");
    let req = Request::builder().method("POST").uri("/talkgroups/radios/import").header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={b}")).body(Body::from(body)).unwrap();
    let res = sdrtrunk_web::app::router(st.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    (status, String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

/// The mapping step's fields: playlist list `i` (`src_i`, as the page sends it back) goes to `map_i`.
fn mapping<'a>(csrf: &'a str, maps: &[&'a str]) -> Vec<(&'a str, &'a str)> {
    const SRC: [&str; 2] = ["P25 Sim", "Other"];
    const KEYS: [(&str, &str); 2] = [("src_0", "map_0"), ("src_1", "map_1")];
    let mut f = vec![("csrf", csrf), ("xml", PLAYLIST)];
    for (i, m) in maps.iter().enumerate() { f.push((KEYS[i].0, SRC[i])); f.push((KEYS[i].1, m)); }
    f
}

#[tokio::test]
async fn upload_shows_every_playlist_list_unmapped() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    let page = send(&st, "GET", "/talkgroups", Some(&cookie), &[]).await.body;
    assert!(page.contains("Import radio names from SDRTrunk"), "{page}");
    let (status, body) = upload(&st, &cookie, &[("csrf", &csrf)], Some(PLAYLIST)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (a, b) = (body.find("P25 Sim").expect("first list"), body.find("Other").expect("second list"));
    assert!(a < b, "file order");
    assert_eq!(body.matches(r#"<option value="" selected>Choose Alias List...</option>"#).count(), 2, "{body}");
    assert!(body.contains("Don't import") && body.contains(r#"name="map_0" required"#), "{body}");
    assert!(st.db.radio_names(SYS).is_empty(), "nothing stored before mapping");
    let (_, body) = upload(&st, &cookie, &[("csrf", &csrf)], Some("<nope/>")).await;
    assert!(body.contains("not an sdrtrunk playlist"), "{body}");
}

#[tokio::test]
async fn every_list_must_be_mapped_or_skipped() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    let (_, body) = upload(&st, &cookie, &mapping(&csrf, &[SYS, ""]), None).await;
    assert!(body.contains("Choose an alias list or Don't import for every playlist list."), "{body}");
    assert!(st.db.radio_names(SYS).is_empty());
    upload(&st, &cookie, &mapping(&csrf, &[SYS, "-"]), None).await;
    assert_eq!(st.db.radio_names(SYS), vec![rn(1, "New"), rn(733_730, "K315")], "Other skipped");
    let (_, body) = upload(&st, &cookie, &mapping(&csrf, &["-", SYS]), None).await;
    assert!(!body.contains("radio range"), "the range is in P25 Sim, which was not imported: {body}");
    let (status, _) = upload(&st, &cookie, &mapping(&csrf, &["nope", "-"]), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unknown list");
}

#[tokio::test]
async fn two_playlist_lists_into_one_first_wins() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    let (_, body) = upload(&st, &cookie, &mapping(&csrf, &[SYS, SYS]), None).await;
    assert_eq!(st.db.radio_names(SYS), vec![rn(1, "New"), rn(9, "Elsewhere"), rn(733_730, "K315")]);
    assert!(body.contains("radio 1 in P25: New in P25 Sim, Different in Other; kept New"), "{body}");
}

#[tokio::test]
async fn reimport_keeps_names_unless_chosen() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    st.db.save_radio_name(SYS, &rn(733_730, "K316")).unwrap();
    st.db.save_radio_name(SYS, &rn(42, "Only ours")).unwrap();
    let (status, body) = upload(&st, &cookie, &mapping(&csrf, &[SYS, "-"]), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(st.db.radio_names(SYS), vec![rn(1, "New"), rn(42, "Only ours"), rn(733_730, "K316")], "added, nothing overwritten");
    assert!(body.contains("Names that differ") && body.contains("K315") && body.contains("1 radio range skipped"), "{body}");
    assert!(body.contains(r#"name="choice_0" value="local" checked"#), "Keep is the default: {body}");
    assert_eq!(policy(&st).lock().unwrap().radios.get(&1).map(String::as_str), Some("New"));
    let c = Some(cookie.as_str());
    let apply = |choice: &'static str| [("csrf", csrf.as_str()), ("conflict", "0"), ("list_0", SYS), ("radio_0", "733730"),
        ("pl_name_0", "K315"), ("choice_0", choice)];
    let r = send(&st, "POST", "/talkgroups/radios/import/apply", c, &apply("local")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(st.db.radio_names(SYS)[2], rn(733_730, "K316"));
    send(&st, "POST", "/talkgroups/radios/import/apply", c, &apply("playlist")).await;
    assert_eq!(st.db.radio_names(SYS)[2], rn(733_730, "K315"));
    assert_eq!(policy(&st).lock().unwrap().radios.get(&733_730).map(String::as_str), Some("K315"));
}

#[tokio::test]
async fn import_needs_talkgroups_import() {
    let st = state();
    let (cookie, csrf) = login_with(&st, "helper", &[Function::TalkgroupsEdit]);
    let (status, _) = upload(&st, &cookie, &mapping(&csrf, &[SYS, "-"]), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let r = send(&st, "POST", "/talkgroups/radios/import/apply", Some(&cookie),
        &[("csrf", &csrf), ("conflict", "0"), ("list_0", SYS), ("radio_0", "1"), ("pl_name_0", "X"), ("choice_0", "playlist")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(st.db.radio_names(SYS).is_empty());
}

#[tokio::test]
async fn a_large_playlist_uploads() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    // Over axum's 2 MB default body limit, like a playlist with thousands of aliases and streams.
    let big = PLAYLIST.replace("</playlist>", &format!("<!-- {} --></playlist>", "x".repeat(3_000_000)));
    let (status, body) = upload(&st, &cookie, &[("csrf", &csrf)], Some(&big)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("P25 Sim") && !body.contains("not an sdrtrunk playlist"), "{}", &body[..body.len().min(500)]);
}
