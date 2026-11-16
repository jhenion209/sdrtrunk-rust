mod common;
use axum::http::StatusCode;
use common::*;
use sdrtrunk_web::auth::Role;

fn id_of(st: &sdrtrunk_web::app::AppState, name: &str) -> i64 { st.db.users().into_iter().find(|u| u.username == name).unwrap().id }

#[tokio::test]
async fn admin_forbidden_from_users() {
    let st = state();
    login_as(&st, "owner", Role::Owner);
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    assert_eq!(send(&st, "GET", "/users", Some(&cookie), &[]).await.status, StatusCode::FORBIDDEN);
    let r = send(&st, "POST", "/users", Some(&cookie), &[("csrf", &csrf), ("username", "evil"), ("password", PW)]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let owner = id_of(&st, "owner").to_string();
    let r = send(&st, "POST", &format!("/users/{owner}/password"), Some(&cookie), &[("csrf", &csrf), ("password", "attacker password")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(st.db.users().len(), 2);
    let home = send(&st, "GET", "/", Some(&cookie), &[]).await.body;
    assert!(!home.contains("href=\"/users\""), "admins do not see the Users link");
}

#[tokio::test]
async fn owner_adds_and_removes_admin() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    let r = send(&st, "POST", "/users", Some(&cookie), &[("csrf", &csrf), ("username", "alice"), ("password", PW)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let alice = id_of(&st, "alice");
    assert_eq!(st.db.users().iter().find(|u| u.id == alice).unwrap().role, Role::Admin);
    assert!(st.db.verify_login("alice", PW).is_ok());
    let page = send(&st, "GET", "/users", Some(&cookie), &[]).await.body;
    assert!(page.contains("alice") && page.contains("added admin alice"), "audit log shown");
    let r = send(&st, "POST", &format!("/users/{alice}/delete"), Some(&cookie), &[("csrf", &csrf)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(st.db.users().len(), 1);
    assert_eq!(st.db.audit_log(1)[0].action, "removed admin alice");
}

#[tokio::test]
async fn owner_not_removable() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    let id = id_of(&st, "owner");
    let r = send(&st, "POST", &format!("/users/{id}/delete"), Some(&cookie), &[("csrf", &csrf)]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("cannot be removed"), "{}", r.body);
    assert_eq!(st.db.user_count(), 1);
}

#[tokio::test]
async fn reset_password_logs_admin_out() {
    let st = state();
    let (owner, csrf) = login_as(&st, "owner", Role::Owner);
    let (admin, _) = login_as(&st, "admin", Role::Admin);
    let id = id_of(&st, "admin");
    let r = send(&st, "POST", &format!("/users/{id}/password"), Some(&owner), &[("csrf", &csrf), ("password", "a brand new password")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(send(&st, "GET", "/", Some(&admin), &[]).await.status, StatusCode::SEE_OTHER);
    assert!(st.db.verify_login("admin", "a brand new password").is_ok());
}

#[tokio::test]
async fn account_change_requires_current_password() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    let form = |cur| [("csrf", csrf.as_str()), ("current", cur), ("password", "my new long password"), ("password2", "my new long password")];
    let r = send(&st, "POST", "/account", Some(&cookie), &form("wrong password")).await;
    assert!(r.body.contains("Current password is wrong."), "{}", r.body);
    assert!(st.db.verify_login("owner", PW).is_ok());
    let r = send(&st, "POST", "/account", Some(&cookie), &form(PW)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.cookie.unwrap().starts_with("sdrtrunk_rust_session="), "signed in again with a fresh session");
    assert_eq!(send(&st, "GET", "/", Some(&cookie), &[]).await.status, StatusCode::SEE_OTHER, "old session ended");
    assert!(st.db.verify_login("owner", "my new long password").is_ok());
}

#[tokio::test]
async fn weak_password_rejected() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    let r = send(&st, "POST", "/users", Some(&cookie), &[("csrf", &csrf), ("username", "bob"), ("password", "12345678901")]).await;
    assert!(r.body.contains("at least 12 characters"), "{}", r.body);
    assert_eq!(st.db.user_count(), 1);
    let r = send(&st, "POST", "/account", Some(&cookie), &[("csrf", &csrf), ("current", PW), ("password", "short"), ("password2", "short")]).await;
    assert!(r.body.contains("at least 12 characters"));
}
