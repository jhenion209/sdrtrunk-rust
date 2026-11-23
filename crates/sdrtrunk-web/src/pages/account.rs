//! Change your own password. Every session of the account ends; this browser is signed in again.
use crate::app::{check_csrf, Agent, AppState, Authed, ClientAddr, Secure};
use crate::auth::Session;
use crate::pages::layout::{layout, notice};
use crate::receiver::now_ms;
use axum::extract::{Query, State};
use axum::http::header::SET_COOKIE;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use maud::{html, Markup};
use serde::Deserialize;

fn view(s: &Session, error: Option<&str>, saved: bool) -> Markup {
    layout("Account", Some(&s.user), &s.csrf, html! {
        p { "Signed in as " strong { (s.user.username) } "." }
        (notice(error, saved.then_some("Password changed. Other devices were signed out.")))
        form method="post" action="/account" {
            input type="hidden" name="csrf" value=(s.csrf);
            label { "Current password" input name="current" type="password" autocomplete="current-password" required; }
            label { "New password (12 characters or more)" input name="password" type="password" autocomplete="new-password" required; }
            label { "New password again" input name="password2" type="password" autocomplete="new-password" required; }
            button { "Change password" }
        }
    })
}

#[derive(Deserialize, Default)]
pub struct Saved { saved: Option<u8> }

pub async fn page(Authed(s, _): Authed, Query(q): Query<Saved>) -> Markup { view(&s, None, q.saved.is_some()) }

#[derive(Deserialize)]
pub struct AccountForm { #[serde(default)] csrf: String, current: String, password: String, password2: String }

pub async fn submit(State(st): State<AppState>, Authed(s, _): Authed, Secure(secure): Secure, ClientAddr(ip): ClientAddr, Agent(agent): Agent, Form(f): Form<AccountForm>) -> Response {
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    if st.db.verify_login(&s.user.username, &f.current).is_err() {
        return view(&s, Some("Current password is wrong."), false).into_response();
    }
    if f.password != f.password2 { return view(&s, Some("The new passwords do not match."), false).into_response(); }
    if let Err(e) = st.db.set_password(s.user.id, &f.password) { return view(&s, Some(&e.to_string()), false).into_response(); }
    let now = now_ms();
    st.db.audit(now, &s.user.username, "changed own password");
    let token = st.db.create_session(s.user.id, now, &ip, &agent);
    ([(SET_COOKIE, st.session_cookie(&token, secure))], Redirect::to("/account?saved=1")).into_response()
}
