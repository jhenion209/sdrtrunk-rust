mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use http_body_util::BodyExt;
use tower::ServiceExt;
use sdrtrunk_web::app::{router, AppState};
use sdrtrunk_web::auth::Role;
use sdrtrunk_web::streaming::StreamTalkgroup;

/// One stream fed by talkgroups of two sdrtrunk alias lists; a second stream nobody's alias names.
const PLAYLIST: &str = r#"<playlist version="4">
  <alias name="WPD West" list="SGCo P25"><id type="talkgroup" value="23" protocol="APCO25"/><id type="broadcastChannel" channel="WPD West"/></alias>
  <alias name="KHP" list="KSICS"><id type="talkgroup" value="2368" protocol="APCO25"/><id type="broadcastChannel" channel="WPD West"/></alias>
  <stream type="icecastHTTPConfiguration" description="Patrol West" sample_rate="8000" user_name="source" bitrate="16"
    mount_point="/scan21" host="127.0.0.1" name="WPD West" enabled="true" port="1" delay="0" password="imported-pw" maximum_recording_age="600000"/>
  <stream type="icecastHTTPConfiguration" mount_point="/empty" host="127.0.0.1" name="Nobody" port="1" password="x"/>
</playlist>"#;

async fn upload(st: &AppState, cookie: &str, csrf: &str, xml: &str) -> (StatusCode, String) {
    let b = "XbOuNdArY";
    let body = format!("--{b}\r\nContent-Disposition: form-data; name=\"csrf\"\r\n\r\n{csrf}\r\n\
--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"default.xml\"\r\nContent-Type: text/xml\r\n\r\n{xml}\r\n--{b}--\r\n");
    let req = Request::builder().method("POST").uri("/streams/import").header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={b}")).body(Body::from(body)).unwrap();
    let res = router(st.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    (status, String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

/// Posts the stream form: talkgroups of alias list P25 and of KSICS.
#[allow(clippy::too_many_arguments)]
async fn post(st: &AppState, cookie: &str, csrf: &str, path: &str, password: &str, p25: &str, ksics: &str, mount: &str) -> Resp {
    send(st, "POST", path, Some(cookie), &[("csrf", csrf), ("enabled", "on"), ("name", "WPD West"), ("host", "127.0.0.1"), ("port", "1"),
        ("mount", mount), ("user", "source"), ("password", password), ("bitrate_kbps", "16"), ("sample_rate", "8000"), ("delay_s", "0"),
        ("max_age_min", "10"), ("tg:P25", p25), ("tg:KSICS", ksics)]).await
}

fn tg(list: &str, group: u16) -> StreamTalkgroup { StreamTalkgroup::new(list, group) }

/// The first `html` event of a live table (or what arrived within 3 s).
async fn first_html(st: &AppState, cookie: Option<&str>, path: &str) -> (StatusCode, String) {
    let mut req = Request::get(path);
    if let Some(c) = cookie { req = req.header(header::COOKIE, c); }
    let res = router(st.clone()).oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = res.status();
    let mut body = res.into_body();
    let mut seen = String::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while let Some(Ok(frame)) = body.frame().await {
            if let Some(d) = frame.data_ref() { seen.push_str(&String::from_utf8_lossy(d)); }
            if seen.contains("event: html") && seen.ends_with("\n\n") { break; }
        }
    }).await;
    (status, seen)
}

#[tokio::test]
async fn live_tables() {
    let st = state();
    let (cookie, _) = login_as(&st, "admin", Role::Admin);
    let (code, streams) = first_html(&st, Some(&cookie), "/streams/events").await;
    assert_eq!(code, StatusCode::OK);
    assert!(streams.contains("event: html") && streams.contains("No streams yet"), "{streams}");
    let (_, tuners) = first_html(&st, Some(&cookie), "/tuners/events").await;
    assert!(tuners.contains("event: html") && tuners.contains("Airspy 735863DC-2F67AFE3"), "{tuners}");
    let page = send(&st, "GET", "/tuners", Some(&cookie), &[]).await.body;
    assert!(page.contains(r#"data-live="/tuners/events""#) && page.contains("Airspy 735863DC-2F67AFE3"), "drawn on load too");
    assert!(send(&st, "GET", "/streams", Some(&cookie), &[]).await.body.contains(r#"data-live="/streams/events""#));
    assert_eq!(first_html(&st, None, "/streams/events").await.0, StatusCode::SEE_OTHER, "signed in only");
}

/// Manual check against a real playlist, the way a browser does it (preview, then confirm with the lists the preview
/// chose): SDRTRUNK_PLAYLIST=path cargo test -p sdrtrunk-web --test streams -- --ignored --nocapture
#[tokio::test]
#[ignore]
async fn real_playlist_import() {
    let xml = std::fs::read_to_string(std::env::var("SDRTRUNK_PLAYLIST").expect("SDRTRUNK_PLAYLIST")).unwrap();
    let st = state();
    for l in ["SGCo", "KSICS"] { st.db.create_alias_list(l).unwrap(); }
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    let (code, preview) = upload(&st, &cookie, &csrf, &xml).await;
    println!("preview {code}: {} bytes, {} rows", preview.len(), preview.matches("<tr>").count());
    let mut form = vec![("csrf".to_string(), csrf.clone())];
    for sel in preview.split("<select name=\"map:").skip(1) {
        let name = sel.split('"').next().unwrap().replace("&amp;", "&");
        let chosen = sel.split("</select>").next().unwrap().split("<option value=\"").skip(1)
            .find(|o| o.contains("\" selected")).map(|o| o.split('"').next().unwrap().to_string()).unwrap_or_default();
        println!("map:{name} = {chosen:?}");
        form.push((format!("map:{name}"), chosen));
    }
    let refs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let r = send(&st, "POST", "/streams/import/apply", Some(&cookie), &refs).await;
    println!("apply: {:?} -> {} streams; audit {:?}", r.location, st.db.streams().len(), st.db.audit_log(1));
    assert!(!st.db.streams().is_empty());
}

#[tokio::test]
async fn streams_page_password_policy_and_import() {
    let st = state();
    st.db.create_alias_list("KSICS").unwrap();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    let c = Some(cookie.as_str());

    let r = post(&st, &cookie, &csrf, "/streams/new", "secret-pw", "23, 85", "2368", "/scan21").await;
    assert_eq!((r.status, r.location.as_deref()), (StatusCode::SEE_OTHER, Some("/streams?saved=1")), "{}", r.body);
    let s = st.db.streams()[0].clone();
    assert_eq!((s.password.as_str(), s.members.clone(), s.max_age_ms),
        ("secret-pw", vec![tg("KSICS", 2368), tg("P25", 23), tg("P25", 85)], 600_000), "talkgroups from two alias lists");
    let page = send(&st, "GET", &format!("/streams/{}", s.id), c, &[]).await.body;
    assert!(!page.contains("secret-pw") && page.contains("blank keeps the saved one"), "the password is never shown");
    assert!(page.contains(r#"name="tg:KSICS" value="2368""#) && page.contains(r#"name="tg:P25" value="23, 85""#), "one box per alias list");
    post(&st, &cookie, &csrf, &format!("/streams/{}", s.id), "", "23", "", "/scan21").await;
    let s = st.db.stream(s.id).unwrap();
    assert_eq!((s.password.as_str(), s.members.clone()), ("secret-pw", vec![tg("P25", 23)]), "a blank password keeps the saved one");
    assert_eq!(policy(&st).lock().unwrap().streamed, vec![23], "the running system follows its list's streamed talkgroups");
    let list = send(&st, "GET", "/streams", c, &[]).await.body;
    assert!(list.contains("127.0.0.1:1/scan21") && list.contains("P25: 23"), "{list}");
    assert!(send(&st, "GET", "/", c, &[]).await.body.contains("streams connected"), "dashboard figure");
    let r = post(&st, &cookie, &csrf, "/streams/new", "pw", "23", "", "scan").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Mounts start with /"));
    let r = post(&st, &cookie, &csrf, "/streams/new", "pw", "", "x1", "/x").await;
    assert!(r.body.contains("KSICS: enter talkgroup numbers"), "the bad box is named");

    let (code, preview) = upload(&st, &cookie, &csrf, PLAYLIST).await;
    assert_eq!(code, StatusCode::OK);
    assert!(preview.contains("2 Icecast streams found") && preview.contains("replaces the saved stream") && preview.contains("skipped"), "{preview}");
    assert!(preview.contains(r#"name="map:KSICS""#) && preview.contains(r#"name="map:SGCo P25""#), "a choice per sdrtrunk alias list");
    assert!(preview.contains(r#"<option value="KSICS" selected>"#), "same-named list chosen for you");
    assert!(!preview.contains("imported-pw"), "passwords stay off the page");
    assert_eq!(st.db.streams().len(), 1, "nothing saved before confirming");
    let r = send(&st, "POST", "/streams/import/apply", c, &[("csrf", &csrf), ("map:SGCo P25", "P25"), ("map:KSICS", "KSICS")]).await;
    assert_eq!(r.location.as_deref(), Some("/streams?imported=1"));
    assert!(send(&st, "GET", "/streams?imported=1", c, &[]).await.body.contains("Imported 1 stream. The streams restarted."));
    assert!(send(&st, "GET", "/streams?imported=0", c, &[]).await.body.contains("No streams imported"));
    let all = st.db.streams();
    assert_eq!(all.len(), 1, "same server and mount: replaced; the stream without talkgroups is skipped");
    assert_eq!((all[0].password.as_str(), all[0].description.as_str(), all[0].members.clone()),
        ("imported-pw", "Patrol West", vec![tg("KSICS", 2368), tg("P25", 23)]));
    assert!(upload(&st, &cookie, &csrf, "<html/>").await.1.contains("Could not read the file"));

    let r = send(&st, "POST", &format!("/streams/{}/delete", all[0].id), c, &[("csrf", &csrf)]).await;
    assert_eq!(r.location.as_deref(), Some("/streams?saved=1"));
    assert!(st.db.streams().is_empty());
    assert!(policy(&st).lock().unwrap().streamed.is_empty());
}
