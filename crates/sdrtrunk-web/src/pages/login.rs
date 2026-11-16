//! Sign in and sign out.
use crate::app::{check_csrf, AppState, Authed, ClientAddr};
use crate::pages::layout::{layout, notice};
use crate::receiver::now_ms;
use axum::extract::State;
use axum::http::header::SET_COOKIE;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use maud::{html, Markup};
use serde::Deserialize;

fn form(error: Option<&str>) -> Markup {
    layout("Sign in", None, "", html! {
        (notice(error, None))
        form method="post" action="/login" {
            label { "Username" input name="username" autocomplete="username" required autofocus; }
            label { "Password" input name="password" type="password" autocomplete="current-password" required; }
            button { "Sign in" }
        }
    })
}

pub async fn page() -> Markup { form(None) }

#[derive(Deserialize)]
pub struct LoginForm { username: String, password: String }

pub async fn submit(State(st): State<AppState>, ClientAddr(ip): ClientAddr, Form(f): Form<LoginForm>) -> Response {
    let now = now_ms();
    // A name no account can have is a failed attempt for the address only; never store arbitrary input as a key.
    let ku = if crate::auth::valid_username(&f.username) { format!("user:{}", f.username) } else { String::new() };
    let ki = format!("ip:{ip}");
    let keys_all = [ku.as_str(), ki.as_str()];
    let keys: &[&str] = if ku.is_empty() { &keys_all[1..] } else { &keys_all };
    if let Err(e) = st.throttle.check(keys, now) { return form(Some(&e.to_string())).into_response(); }
    match st.db.verify_login(&f.username, &f.password) {
        Ok(user) => {
            st.throttle.succeeded(keys);
            let token = st.db.create_session(user.id, now);
            st.db.audit(now, &user.username, "login");
            ([(SET_COOKIE, st.session_cookie(&token))], Redirect::to("/")).into_response()
        }
        Err(e) => {
            st.throttle.failed(keys, now);
            form(Some(&e.to_string())).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct CsrfForm { #[serde(default)] pub csrf: String }

pub async fn logout(State(st): State<AppState>, Authed(s, token): Authed, Form(f): Form<CsrfForm>) -> Response {
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    st.db.end_session(&token);
    st.db.audit(now_ms(), &s.user.username, "logout");
    ([(SET_COOKIE, st.clear_cookie())], Redirect::to("/login")).into_response()
}
