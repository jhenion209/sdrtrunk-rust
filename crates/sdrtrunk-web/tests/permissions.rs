mod common;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use http_body_util::BodyExt;
use sdrtrunk_web::app::{router, AppState};
use sdrtrunk_web::auth::{Function, Role};
use tower::ServiceExt;

const TUNER: &str = "735863DC-2F67AFE3";
const REFUSED: &str = "You are not allowed to ";

/// Every covered POST route and the function it needs (the spec's table). `true`: multipart.
fn routes() -> Vec<(String, Function, bool)> {
    use Function::*;
    vec![
        ("/systems/new".into(), SystemsAdd, false),
        ("/systems/1".into(), SystemsEdit, false),
        ("/systems/1/delete".into(), SystemsDelete, false),
        (format!("/tuners/{TUNER}"), TunersEdit, false),
        ("/talkgroups".into(), TalkgroupsEdit, false),
        ("/talkgroups/27/delete".into(), TalkgroupsEdit, false),
        ("/talkgroups/unlisted".into(), TalkgroupsEdit, false),
        ("/talkgroups/record".into(), TalkgroupsEdit, false),
        ("/talkgroups/reviewed".into(), TalkgroupsEdit, false),
        ("/talkgroups/27/reviewed".into(), TalkgroupsEdit, false),
        ("/talkgroups/import".into(), TalkgroupsImport, true),
        ("/talkgroups/import/apply".into(), TalkgroupsImport, false),
        ("/streams/new".into(), StreamsAdd, false),
        ("/streams/1".into(), StreamsEdit, false),
        ("/streams/1/delete".into(), StreamsDelete, false),
        ("/streams/import".into(), StreamsImport, true),
        ("/streams/import/apply".into(), StreamsImport, false),
        ("/recordings".into(), RecordingsEdit, false),
    ]
}

/// POSTs an empty multipart body (the handler must refuse before reading it).
async fn send_multipart(st: &AppState, path: &str, cookie: &str) -> (StatusCode, String) {
    let req = Request::builder().method("POST").uri(path).header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "multipart/form-data; boundary=X").body(Body::from("--X--\r\n")).unwrap();
    let res = router(st.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    (status, String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

async fn post(st: &AppState, path: &str, multipart: bool, cookie: &str, csrf: &str) -> (StatusCode, String) {
    if multipart { return send_multipart(st, path, cookie).await; }
    // recording_dir: /recordings rejects a body without it (422) before the handler runs.
    let r = send(st, "POST", path, Some(cookie), &[("csrf", csrf), ("system", SYS), ("recording_dir", "/srv/recordings")]).await;
    (r.status, r.body)
}

#[tokio::test]
async fn every_change_route_needs_its_function() {
    assert_eq!(routes().len(), 18);
    for (path, f, multipart) in routes() {
        let st = state();
        let (cookie, csrf) = login_with(&st, "jordan", &[]);
        let (status, body) = post(&st, &path, multipart, &cookie, &csrf).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path} without {f:?}");
        assert!(body.contains(&format!("{REFUSED}{}. Ask the owner.", f.label().to_lowercase())), "{path}: {body}");
        let (cookie, csrf) = login_with(&st, "jordan", &[f]);
        let (_, body) = post(&st, &path, multipart, &cookie, &csrf).await;
        assert!(!body.contains(REFUSED), "{path} with {f:?} must not be refused: {body}");
    }
}

#[tokio::test]
async fn owner_is_never_refused() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    for (path, _, multipart) in routes() {
        let (_, body) = post(&st, &path, multipart, &cookie, &csrf).await;
        assert!(!body.contains(REFUSED), "{path}: {body}");
    }
}

#[tokio::test]
async fn refused_change_changes_nothing() {
    let st = state();
    let (cookie, csrf) = login_with(&st, "jordan", &[Function::SystemsAdd]);
    let r = send(&st, "POST", "/systems/1", Some(&cookie), &[("csrf", csrf.as_str()), ("enabled", "on"), ("system_name", "Renamed"),
        ("site_name", "S"), ("channel_name", "C"), ("primary", "852.1125"), ("failover_ms", "1000"), ("modulation", "c4fm")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(st.db.systems()[0].system_name, SYS);
}

#[tokio::test]
async fn account_page_always_allowed() {
    let st = state();
    let (cookie, csrf) = login_with(&st, "jordan", &[]);
    let r = send(&st, "POST", "/account", Some(&cookie), &[("csrf", csrf.as_str()), ("current", PW),
        ("password", "my new long password"), ("password2", "my new long password")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
}

fn user_id(st: &AppState, name: &str) -> i64 { st.db.users().into_iter().find(|u| u.username == name).unwrap().id }

#[tokio::test]
async fn owner_saves_permissions() {
    let st = state();
    login_with(&st, "jordan", &[]);
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    let id = user_id(&st, "jordan");
    let r = send(&st, "POST", &format!("/users/{id}/allowed"), Some(&cookie),
        &[("csrf", csrf.as_str()), ("function", "systems.edit"), ("function", "tuners.edit"), ("function", "bogus")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(st.db.allowed(id), vec![Function::SystemsEdit, Function::TunersEdit]);
    assert_eq!(st.db.audit_log(1)[0].action, "changed permissions of jordan");
    let page = send(&st, "GET", "/users", Some(&cookie), &[]).await.body;
    assert_eq!(page.matches(" checked").count(), 2, "exactly jordan's two boxes are checked: {page}");
    assert!(page.contains("value=\"systems.edit\" checked") && page.contains("value=\"tuners.edit\" checked"), "{page}");
}

#[tokio::test]
async fn only_owner_sets_permissions() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "boss-admin", Role::Admin);
    let own = user_id(&st, "boss-admin");
    let r = send(&st, "POST", &format!("/users/{own}/allowed"), Some(&cookie), &[("csrf", csrf.as_str())]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(st.db.allowed(own).len(), 11, "unchanged");
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    let owner = user_id(&st, "owner");
    let r = send(&st, "POST", &format!("/users/{owner}/allowed"), Some(&cookie), &[("csrf", csrf.as_str())]).await;
    assert!(r.body.contains("The owner can always do everything."), "{}", r.body);
}

#[tokio::test]
async fn change_applies_next_request() {
    let st = state();
    let (jordan, jcsrf) = login_with(&st, "jordan", &[Function::SystemsEdit]);
    let rename = |name: &'static str, csrf: String| vec![("csrf".to_string(), csrf), ("enabled".into(), "on".into()),
        ("system_name".into(), name.into()), ("site_name".into(), "S".into()), ("channel_name".into(), "C".into()),
        ("primary".into(), "852.1125".into()), ("failover_ms".into(), "1000".into()), ("modulation".into(), "c4fm".into())];
    let form = rename("First", jcsrf.clone());
    let pairs: Vec<(&str, &str)> = form.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    assert_ne!(send(&st, "POST", "/systems/1", Some(&jordan), &pairs).await.status, StatusCode::FORBIDDEN);
    let (owner, ocsrf) = login_as(&st, "owner", Role::Owner);
    let id = user_id(&st, "jordan");
    send(&st, "POST", &format!("/users/{id}/allowed"), Some(&owner), &[("csrf", ocsrf.as_str())]).await;
    let form = rename("Second", jcsrf);
    let pairs: Vec<(&str, &str)> = form.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    assert_eq!(send(&st, "POST", "/systems/1", Some(&jordan), &pairs).await.status, StatusCode::FORBIDDEN, "same session, now refused");
}

#[tokio::test]
async fn pages_show_limits() {
    let st = state();
    let (cookie, _) = login_with(&st, "jordan", &[Function::SystemsEdit]);
    let get = |path: &'static str, cookie: String| { let st = st.clone(); async move { send(&st, "GET", path, Some(&cookie), &[]).await.body } };
    let systems = get("/systems", cookie.clone()).await;
    assert!(!systems.contains("href=\"/systems/new\""), "{systems}");
    let new = get("/systems/new", cookie.clone()).await;
    assert!(new.contains("Read only: ask the owner to allow add systems.") && !new.contains("<form method=\"post\" action=\"/systems/new\""), "{new}");
    let edit = get("/systems/1", cookie.clone()).await;
    assert!(!edit.contains("Read only") && edit.contains("Save and restart this system"), "edit allowed: {edit}");
    assert!(!edit.contains("Delete this system"), "delete not allowed: {edit}");
    let tuner = send(&st, "GET", &format!("/tuners/{TUNER}"), Some(&cookie), &[]).await.body;
    assert!(tuner.contains("Read only: ask the owner to allow edit tuners.") && tuner.contains("<fieldset class=\"lock\" disabled>"), "{tuner}");
    assert!(!tuner.contains(">Save</button>"), "{tuner}");
    let tgs = get("/talkgroups", cookie.clone()).await;
    assert!(tgs.contains("allow edit talkgroups") && !tgs.contains("action=\"/talkgroups/import\""), "{tgs}");
    assert!(!tgs.contains("Save talkgroup"), "{tgs}");
    let streams = get("/streams", cookie.clone()).await;
    assert!(!streams.contains("href=\"/streams/new\"") && !streams.contains("action=\"/streams/import\""), "{streams}");
    let rec = get("/recordings", cookie.clone()).await;
    assert!(rec.contains("allow recordings and event log settings") && !rec.contains("Save and restart all systems"), "{rec}");
    let (owner, _) = login_as(&st, "owner", Role::Owner);
    for path in ["/systems", "/systems/new", "/systems/1", "/talkgroups", "/streams", "/recordings"] {
        let body = get(path, owner.clone()).await;
        assert!(!body.contains("Read only"), "{path}: {body}");
    }
    let tuner = send(&st, "GET", &format!("/tuners/{TUNER}"), Some(&owner), &[]).await.body;
    assert!(!tuner.contains("Read only") && tuner.contains(">Save</button>"), "{tuner}");
}
