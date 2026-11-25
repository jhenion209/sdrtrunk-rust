//! Connected hosts: the owner sees every signed-in session and can end one; admins cannot see the page.
mod common;
use axum::http::StatusCode;
use common::*;
use sdrtrunk_web::auth::Role;
use std::sync::Arc;

#[tokio::test]
async fn owner_sees_sessions() {
    let st = state();
    let (owner, _) = login_as(&st, "owner", Role::Owner);
    login_as(&st, "admin2", Role::Admin);
    let r = send(&st, "GET", "/hosts", Some(&owner), &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("owner") && r.body.contains("admin2"), "{}", r.body);
    assert!(r.body.contains("local"), "the address");
    assert_eq!(r.body.matches("(this one)").count(), 1);
    assert!(r.body.contains(r#"href="/hosts""#), "in the menu");
}

#[tokio::test]
async fn admin_is_refused() {
    let st = state();
    let (admin, _) = login_as(&st, "admin2", Role::Admin);
    assert_eq!(send(&st, "GET", "/hosts", Some(&admin), &[]).await.status, StatusCode::FORBIDDEN);
    let r = send(&st, "GET", "/", Some(&admin), &[]).await;
    assert!(!r.body.contains(r#"href="/hosts""#), "not in an admin's menu");
}

fn row_of(st: &sdrtrunk_web::app::AppState, user: &str) -> i64 {
    st.db.sessions(sdrtrunk_web::receiver::now_ms()).into_iter().find(|r| r.username == user).unwrap().row
}

#[tokio::test]
async fn sign_out_ends_that_session() {
    let st = state();
    let (owner, csrf) = login_as(&st, "owner", Role::Owner);
    let (admin, _) = login_as(&st, "admin2", Role::Admin);
    let row = row_of(&st, "admin2");
    let r = send(&st, "POST", &format!("/hosts/{row}/end"), Some(&owner), &[("csrf", &csrf)]).await;
    assert_eq!(r.location.as_deref(), Some("/hosts"));
    assert_eq!(send(&st, "GET", "/", Some(&admin), &[]).await.location.as_deref(), Some("/login"), "signed out");
    assert_eq!(send(&st, "GET", "/", Some(&owner), &[]).await.status, StatusCode::OK, "the owner stays signed in");
    assert!(st.db.audit_log(10).iter().any(|a| a.username == "owner" && a.action == "ended admin2's session from local"));
}

#[tokio::test]
async fn sign_out_needs_csrf() {
    let st = state();
    let (owner, _) = login_as(&st, "owner", Role::Owner);
    login_as(&st, "admin2", Role::Admin);
    let row = row_of(&st, "admin2");
    let r = send(&st, "POST", &format!("/hosts/{row}/end"), Some(&owner), &[("csrf", "bad")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(st.db.sessions(sdrtrunk_web::receiver::now_ms()).len(), 2, "nothing ended");
}

#[tokio::test]
async fn ending_own_session_goes_to_login() {
    let st = state();
    let (owner, csrf) = login_as(&st, "owner", Role::Owner);
    let row = row_of(&st, "owner");
    let r = send(&st, "POST", &format!("/hosts/{row}/end"), Some(&owner), &[("csrf", &csrf)]).await;
    assert_eq!(r.location.as_deref(), Some("/login"));
    assert!(r.cookie.unwrap().contains("Max-Age=0"));
}

#[tokio::test]
async fn a_session_already_gone_is_not_found() {
    let st = state();
    let (owner, csrf) = login_as(&st, "owner", Role::Owner);
    let r = send(&st, "POST", "/hosts/999/end", Some(&owner), &[("csrf", &csrf)]).await;
    assert_eq!(r.location.as_deref(), Some("/hosts?gone=1"));
}

#[tokio::test]
async fn web_access_box_shows_addresses_and_certificate() {
    let mut st = state();
    st.web = Arc::new(sdrtrunk_web::https::WebStatus {
        https: Some("0.0.0.0:443".parse().unwrap()), source: "own", ..Default::default()
    });
    let (owner, _) = login_as(&st, "owner", Role::Owner);
    let body = send(&st, "GET", "/hosts", Some(&owner), &[]).await.body;
    for want in ["HTTPS: https://0.0.0.0:443", "HTTP: off", "Certificate: own"] {
        assert!(body.contains(want), "{want} missing: {body}");
    }
}
