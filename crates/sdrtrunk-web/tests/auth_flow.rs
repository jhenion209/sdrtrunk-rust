mod common;
use axum::http::StatusCode;
use common::*;
use sdrtrunk_web::auth::Role;

#[tokio::test]
async fn unauthenticated_redirects_to_login() {
    let st = state();
    let r = send(&st, "GET", "/", None, &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location.as_deref(), Some("/login"));
    let r = send(&st, "GET", "/", Some("sdrtrunk_rust_session=forged"), &[]).await;
    assert_eq!(r.location.as_deref(), Some("/login"));
}

#[tokio::test]
async fn setup_requires_code() {
    let st = state();
    assert_eq!(send(&st, "GET", "/setup", None, &[]).await.status, StatusCode::OK);
    let form = |code| [("code", code), ("username", "owner"), ("password", PW), ("password2", PW)];
    let r = send(&st, "POST", "/setup", None, &form("WRNG-CODE")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("setup code"), "{}", r.body);
    assert_eq!(st.db.user_count(), 0);
    let r = send(&st, "POST", "/setup", None, &form("ABCD-EFGH")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.cookie.unwrap().starts_with("sdrtrunk_rust_session="));
    assert_eq!(st.db.users()[0].role, Role::Owner);
    assert!(st.setup_code.lock().unwrap().is_none(), "code is one-time");
}

#[tokio::test]
async fn setup_gone_after_owner() {
    let st = state();
    login_as(&st, "owner", Role::Owner);
    assert_eq!(send(&st, "GET", "/setup", None, &[]).await.status, StatusCode::NOT_FOUND);
    let r = send(&st, "POST", "/setup", None, &[("code", "ABCD-EFGH"), ("username", "x"), ("password", PW), ("password2", PW)]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(st.db.user_count(), 1);
}

#[tokio::test]
async fn login_logout() {
    let st = state();
    st.db.create_user("owner", PW, Role::Owner, 0).unwrap();
    assert_eq!(send(&st, "GET", "/login", None, &[]).await.status, StatusCode::OK);
    let r = send(&st, "POST", "/login", None, &[("username", "owner"), ("password", PW)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let set = r.cookie.unwrap();
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict") && !set.contains("Secure"), "{set}");
    let cookie = set.split(';').next().unwrap().to_string();
    let page = send(&st, "GET", "/", Some(&cookie), &[]).await;
    assert_eq!(page.status, StatusCode::OK);
    let csrf = st.db.session(cookie.trim_start_matches("sdrtrunk_rust_session="), sdrtrunk_web::receiver::now_ms()).unwrap().csrf;
    let r = send(&st, "POST", "/logout", Some(&cookie), &[("csrf", &csrf)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(send(&st, "GET", "/", Some(&cookie), &[]).await.status, StatusCode::SEE_OTHER);
    let actions: Vec<String> = st.db.audit_log(10).into_iter().map(|a| a.action).collect();
    assert_eq!(actions, vec!["logout", "login"]);
}

#[tokio::test]
async fn bad_password_rejected() {
    let st = state();
    st.db.create_user("owner", PW, Role::Owner, 0).unwrap();
    let r = send(&st, "POST", "/login", None, &[("username", "owner"), ("password", "not the password")]).await;
    assert!(r.cookie.is_none());
    assert!(r.body.contains("Wrong username or password"));
}

/// A sign-in request (the owner's name and password) with whatever the test adds.
fn sign_in() -> axum::http::request::Builder {
    axum::http::Request::builder().method("POST").uri("/login")
        .header(axum::http::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
}
fn sign_in_body() -> axum::body::Body { axum::body::Body::from(format!("username=owner&password={}", PW.replace(' ', "+"))) }
fn peer(ip: &str) -> axum::extract::ConnectInfo<std::net::SocketAddr> {
    axum::extract::ConnectInfo(std::net::SocketAddr::new(ip.parse().unwrap(), 50000))
}

#[tokio::test]
async fn login_cookie_is_secure_over_https() {
    let st = state();
    st.db.create_user("owner", PW, Role::Owner, 0).unwrap();
    let r = send_req(&st, sign_in().extension(sdrtrunk_web::app::Https).body(sign_in_body()).unwrap()).await;
    assert!(r.cookie.unwrap().contains("; Secure"));
}

#[tokio::test]
async fn login_cookie_is_plain_over_http() {
    let st = state();
    st.db.create_user("owner", PW, Role::Owner, 0).unwrap();
    let r = send(&st, "POST", "/login", None, &[("username", "owner"), ("password", PW)]).await;
    assert!(!r.cookie.unwrap().contains("Secure"));
}

#[tokio::test]
async fn trusted_proxy_says_https() {
    let mut st = state();
    st.trusted = std::sync::Arc::new(vec![sdrtrunk_web::web_config::Proxy::parse("192.168.104.0/24").unwrap()]);
    st.db.create_user("owner", PW, Role::Owner, 0).unwrap();
    let r = send_req(&st, sign_in().extension(peer("192.168.104.7")).header("x-forwarded-proto", "https")
        .body(sign_in_body()).unwrap()).await;
    assert!(r.cookie.unwrap().contains("; Secure"));
}

#[tokio::test]
async fn untrusted_proxy_header_is_ignored() {
    let mut st = state();
    st.trusted = std::sync::Arc::new(vec![sdrtrunk_web::web_config::Proxy::parse("192.168.104.0/24").unwrap()]);
    st.db.create_user("owner", PW, Role::Owner, 0).unwrap();
    let r = send_req(&st, sign_in().extension(peer("203.0.113.1")).header("x-forwarded-proto", "https")
        .body(sign_in_body()).unwrap()).await;
    assert!(!r.cookie.unwrap().contains("Secure"));
}

#[tokio::test]
async fn sign_out_over_https_clears_a_secure_cookie() {
    let st = state();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    let r = send_req(&st, axum::http::Request::builder().method("POST").uri("/logout").extension(sdrtrunk_web::app::Https)
        .header(axum::http::header::COOKIE, &cookie)
        .header(axum::http::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(format!("csrf={csrf}"))).unwrap()).await;
    let c = r.cookie.unwrap();
    assert!(c.contains("Max-Age=0") && c.contains("; Secure"), "{c}");
}

#[tokio::test]
async fn csrf_required() {
    let st = state();
    let (cookie, _) = login_as(&st, "owner", Role::Owner);
    let r = send(&st, "POST", "/logout", Some(&cookie), &[("csrf", "bad")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(send(&st, "GET", "/", Some(&cookie), &[]).await.status, StatusCode::OK, "still logged in");
}

#[tokio::test]
async fn login_throttled() {
    let st = state();
    st.db.create_user("owner", PW, Role::Owner, 0).unwrap();
    for _ in 0..5 { send(&st, "POST", "/login", None, &[("username", "owner"), ("password", "wrong password!")]).await; }
    let r = send(&st, "POST", "/login", None, &[("username", "owner"), ("password", PW)]).await;
    assert!(r.cookie.is_none());
    assert!(r.body.contains("Too many failed sign-ins"), "{}", r.body);
}
