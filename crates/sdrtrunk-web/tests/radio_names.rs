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
