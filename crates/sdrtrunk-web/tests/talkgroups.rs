mod common;
use axum::http::StatusCode;
use common::*;
use sdrtrunk_web::auth::Role;

#[tokio::test]
async fn add_edit_delete() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    let c = Some(cookie.as_str());
    let r = send(&st, "POST", "/talkgroups", c, &[("csrf", &csrf), ("list", SYS), ("number", "85"), ("name", "Fire"), ("record", "on")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let t = st.db.talkgroups(SYS);
    assert_eq!((t[0].number, t[0].name.as_str(), t[0].record), (85, "Fire", true));
    send(&st, "POST", "/talkgroups", c, &[("csrf", &csrf), ("list", SYS), ("number", "85"), ("name", "Fire Dispatch")]).await;
    let t = st.db.talkgroups(SYS);
    assert_eq!((t.len(), t[0].name.as_str(), t[0].record), (1, "Fire Dispatch", false), "unchecked box = do not record");
    let page = send(&st, "GET", "/talkgroups", c, &[]).await;
    assert!(page.body.contains("Fire Dispatch"));
    assert_eq!(st.db.audit_log(1)[0].action, "talkgroup 85 on P25 saved");
    assert_eq!(policy(&st).lock().unwrap().talkgroups.clone(), st.db.talkgroups(SYS), "recorder sees edits without a restart");
    let r = send(&st, "POST", "/talkgroups/85/delete", c, &[("csrf", &csrf), ("list", SYS)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(st.db.talkgroups(SYS).is_empty());
    assert!(policy(&st).lock().unwrap().talkgroups.is_empty());
    assert_eq!(st.db.audit_log(1)[0].action, "talkgroup 85 on P25 deleted");
    assert_eq!(send(&st, "POST", "/talkgroups/85/delete", c, &[("csrf", "x"), ("list", SYS)]).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn record_unlisted_switch() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    let c = Some(cookie.as_str());
    let page = send(&st, "GET", "/talkgroups", c, &[]).await;
    assert!(page.body.contains("Record talkgroups not listed here"));
    assert!(st.db.record_unlisted(SYS), "on by default");
    let r = send(&st, "POST", "/talkgroups/unlisted", c, &[("csrf", &csrf), ("list", SYS)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(!st.db.record_unlisted(SYS));
    assert!(!policy(&st).lock().unwrap().record_unlisted, "recorder sees it without a restart");
    assert_eq!(st.db.audit_log(1)[0].action, "stopped recording unlisted talkgroups on P25");
    send(&st, "POST", "/talkgroups/unlisted", c, &[("csrf", &csrf), ("list", SYS), ("record_unlisted", "on")]).await;
    assert!(st.db.record_unlisted(SYS) && policy(&st).lock().unwrap().record_unlisted);
    assert_eq!(send(&st, "POST", "/talkgroups/unlisted", c, &[("csrf", "x"), ("list", SYS)]).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn edit_loads_entry_into_form() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    let c = Some(cookie.as_str());
    send(&st, "POST", "/talkgroups", c, &[("csrf", &csrf), ("list", SYS), ("number", "51"), ("name", "Sheriff Ops")]).await;
    let list = send(&st, "GET", "/talkgroups", c, &[]).await;
    assert!(list.body.contains("href=\"/talkgroups?list=P25&amp;edit=51\""), "edit link per row");
    let page = send(&st, "GET", "/talkgroups?edit=51", c, &[]).await;
    assert!(page.body.contains("Edit talkgroup 51"));
    assert!(page.body.contains("value=\"Sheriff Ops\"") && page.body.contains("value=\"51\""));
    assert!(!page.body.contains("name=\"record\" checked"), "record flag off is shown off");
    let unknown = send(&st, "GET", "/talkgroups?edit=999", c, &[]).await;
    assert!(unknown.body.contains("Add talkgroup"), "unknown number: empty add form");
}

#[tokio::test]
async fn invalid_number_rejected() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    for n in ["0", "70000", "x", ""] {
        let r = send(&st, "POST", "/talkgroups", Some(&cookie), &[("csrf", &csrf), ("list", SYS), ("number", n), ("name", "Bad")]).await;
        assert_eq!(r.status, StatusCode::OK, "{n}");
        assert!(r.body.contains("Talkgroup numbers are 1 to 65535."), "{n}: {}", r.body);
    }
    let r = send(&st, "POST", "/talkgroups", Some(&cookie), &[("csrf", &csrf), ("list", SYS), ("number", "5"), ("name", "  ")]).await;
    assert!(r.body.contains("Names are 1 to 64 characters."));
    assert!(st.db.talkgroups(SYS).is_empty());
}

#[tokio::test]
async fn name_escaped() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    send(&st, "POST", "/talkgroups", Some(&cookie), &[("csrf", &csrf), ("list", SYS), ("number", "7"), ("name", "<script>alert(1)</script>")]).await;
    let page = send(&st, "GET", "/talkgroups", Some(&cookie), &[]).await.body;
    assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"), "{page}");
    assert!(!page.contains("<script>alert(1)"));
}

#[tokio::test]
async fn batch_record_setting() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    let c = Some(cookie.as_str());
    for (n, name) in [(1, "A"), (3, "B"), (5, "C")] {
        st.db.upsert_talkgroup(SYS, &sdrtrunk_web::db::Talkgroup { number: n, name: name.into(), record: true }).unwrap();
    }
    let page = send(&st, "GET", "/talkgroups", c, &[]).await;
    assert!(page.body.contains("form=\"batch\"") && page.body.contains("Don't record selected"));
    let r = send(&st, "POST", "/talkgroups/record", c, &[("csrf", &csrf), ("list", SYS), ("n", "1"), ("n", "5"), ("record", "off")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let rec: Vec<bool> = st.db.talkgroups(SYS).iter().map(|t| t.record).collect();
    assert_eq!(rec, vec![false, true, false]);
    assert!(!policy(&st).lock().unwrap().talkgroups[0].record, "recorder sees it live");
    assert_eq!(st.db.audit_log(1)[0].action, "set 2 talkgroups on P25 to not record");
    send(&st, "POST", "/talkgroups/record", c, &[("csrf", &csrf), ("list", SYS), ("n", "1"), ("record", "on")]).await;
    assert!(st.db.talkgroups(SYS)[0].record);
    assert_eq!(send(&st, "POST", "/talkgroups/record", c, &[("csrf", &csrf), ("list", SYS), ("n", "x"), ("record", "on")]).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(send(&st, "POST", "/talkgroups/record", c, &[("csrf", "bad"), ("list", SYS), ("n", "1"), ("record", "on")]).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn sortable_columns() {
    let st = state();
    let (cookie, _) = login_as(&st, "admin", Role::Admin);
    let c = Some(cookie.as_str());
    for (n, name, rec) in [(1, "Zulu", true), (3, "Alpha", false), (5, "Mike", true)] {
        st.db.upsert_talkgroup(SYS, &sdrtrunk_web::db::Talkgroup { number: n, name: name.into(), record: rec }).unwrap();
    }
    let order = |body: &str| { let mut v: Vec<(usize, &str)> = ["Zulu", "Alpha", "Mike"].iter().map(|n| (body.find(n).unwrap(), *n)).collect(); v.sort(); v.into_iter().map(|(_, n)| n).collect::<Vec<_>>() };
    let page = send(&st, "GET", "/talkgroups", c, &[]).await.body;
    assert_eq!(order(&page), vec!["Zulu", "Alpha", "Mike"], "by talkgroup ID by default");
    assert!(page.contains("href=\"/talkgroups?list=P25&amp;sort=name&amp;dir=asc\""), "header links");
    assert_eq!(order(&send(&st, "GET", "/talkgroups?sort=name&dir=asc", c, &[]).await.body), vec!["Alpha", "Mike", "Zulu"]);
    assert_eq!(order(&send(&st, "GET", "/talkgroups?sort=name&dir=desc", c, &[]).await.body), vec!["Zulu", "Mike", "Alpha"]);
    assert_eq!(order(&send(&st, "GET", "/talkgroups?sort=record&dir=asc", c, &[]).await.body), vec!["Alpha", "Zulu", "Mike"], "not recorded first, then by ID");
    assert_eq!(order(&send(&st, "GET", "/talkgroups?sort=number&dir=desc", c, &[]).await.body), vec!["Mike", "Alpha", "Zulu"]);
    st.db.import_talkgroups(SYS, &[sdrtrunk_web::db::Talkgroup { number: 9, name: "Bravo".into(), record: true }]).unwrap();
    let body = send(&st, "GET", "/talkgroups?sort=name&dir=desc", c, &[]).await.body;
    assert!(body.find("Bravo").unwrap() < body.find("Zulu").unwrap(), "NEW stays pinned whatever the sort");
}
