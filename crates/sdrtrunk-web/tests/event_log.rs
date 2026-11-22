mod common;
use axum::http::StatusCode;
use chrono::{Local, TimeZone};
use common::*;
use sdrtrunk_p25::trunk::CallGrant;
use sdrtrunk_p25::tsbk_msg::Channel;
use sdrtrunk_web::auth::Role;
use sdrtrunk_web::db::Settings;
use sdrtrunk_web::event_log::EventLog;
use std::sync::Arc;

const DAY: &str = "from=2026-11-20T00%3A00&to=2026-11-20T23%3A59";

fn ms(h: u32, m: u32) -> i64 { Local.with_ymd_and_hms(2026, 11, 20, h, m, 0).single().unwrap().timestamp_millis() }

/// Settings point at a fresh folder holding two calls: TG 27 from radio 911 and TG 41 from radio 711035.
fn with_calls(st: &sdrtrunk_web::app::AppState, name: &str) {
    let d = std::env::temp_dir().join(format!("sdrtrunk-event-log-page-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let mut l = EventLog::new(Some(d.clone()), SYS, Arc::default(), 1, 3650);
    for (t, group, source) in [(ms(10, 0), 27, 911), (ms(10, 5), 41, 711_035)] {
        l.grant(&CallGrant { group, source, channel: Channel { band: 0, number: 4054 }, hz: 859_737_500, encrypted: false, emergency: false }, t);
        l.close_all(t);
    }
    st.db.save_settings(&Settings { event_log_dir: d.display().to_string(), event_log_days: 3650, ..Settings::default() }).unwrap();
}

#[tokio::test]
async fn event_log_page_requires_login() {
    let st = state();
    let r = send(&st, "GET", "/event-log", None, &[]).await;
    assert_eq!(r.location.as_deref(), Some("/login"));
}

#[tokio::test]
async fn event_log_page_lists_and_filters() {
    let st = state();
    with_calls(&st, "lists");
    let (cookie, _) = login_as(&st, "admin", Role::Admin);
    let r = send(&st, "GET", &format!("/event-log?{DAY}"), Some(&cookie), &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("2 calls") && r.body.contains("711035") && r.body.contains("911"), "{}", r.body);
    let r = send(&st, "GET", &format!("/event-log?{DAY}&radios=911"), Some(&cookie), &[]).await;
    assert!(r.body.contains("1 call,") && !r.body.contains("711035"), "{}", r.body);
    let r = send(&st, "GET", &format!("/event-log?{DAY}&groups=41"), Some(&cookie), &[]).await;
    assert!(r.body.contains("1 call,") && r.body.contains("711035"), "{}", r.body);
}

#[tokio::test]
async fn event_log_page_rejects_bad_input() {
    let st = state();
    with_calls(&st, "bad");
    let (cookie, _) = login_as(&st, "admin", Role::Admin);
    let r = send(&st, "GET", &format!("/event-log?{DAY}&radios=91a"), Some(&cookie), &[]).await;
    assert!(r.body.contains("Use numbers separated by commas or spaces.") && !r.body.contains("711035"), "{}", r.body);
    let r = send(&st, "GET", "/event-log?from=2026-11-20T10%3A00&to=2026-11-20T09%3A00", Some(&cookie), &[]).await;
    assert!(r.body.contains("To must be after From."), "{}", r.body);
    st.db.save_settings(&Settings { event_log_days: 1, ..st.db.settings() }).unwrap();
    let r = send(&st, "GET", "/event-log?from=2026-11-01T00%3A00&to=2026-11-20T09%3A00", Some(&cookie), &[]).await;
    assert!(r.body.contains("Logs are kept for 1 day."), "{}", r.body);
}

#[tokio::test]
async fn event_log_page_says_when_off() {
    let st = state();
    st.db.save_settings(&Settings { event_log_dir: String::new(), ..Settings::default() }).unwrap();
    let (cookie, _) = login_as(&st, "admin", Role::Admin);
    let r = send(&st, "GET", "/event-log", Some(&cookie), &[]).await;
    assert!(r.body.contains("The call event log is off.") && r.body.contains("href=\"/recordings\""), "{}", r.body);
}
