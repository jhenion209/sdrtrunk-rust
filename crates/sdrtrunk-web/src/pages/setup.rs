//! First run: create the owner account with the one-time code printed in the receiver log.
use crate::app::{AppState, ClientAddr};
use crate::auth::Role;
use crate::pages::layout::{layout, notice};
use crate::receiver::now_ms;
use axum::extract::State;
use axum::http::{header::SET_COOKIE, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use maud::{html, Markup};
use serde::Deserialize;

fn form(error: Option<&str>) -> Markup {
    layout("First-time setup", None, "", html! {
        p { "Create the owner account. The setup code is printed in the receiver log ("
            code { "journalctl -u sdrtrunk-rust" } ")." }
        (notice(error, None))
        form method="post" action="/setup" {
            label { "Setup code" input name="code" autocomplete="off" required autofocus; }
            label { "Owner username" input name="username" autocomplete="username" required; }
            label { "Password (12 characters or more)" input name="password" type="password" autocomplete="new-password" required; }
            label { "Password again" input name="password2" type="password" autocomplete="new-password" required; }
            button { "Create owner" }
        }
    })
}

pub async fn page(State(st): State<AppState>) -> Response {
    if st.db.user_count() > 0 { return StatusCode::NOT_FOUND.into_response(); }
    form(None).into_response()
}

#[derive(Deserialize)]
pub struct SetupForm { code: String, username: String, password: String, password2: String }

pub async fn submit(State(st): State<AppState>, ClientAddr(ip): ClientAddr, Form(f): Form<SetupForm>) -> Response {
    if st.db.user_count() > 0 { return StatusCode::NOT_FOUND.into_response(); }
    let now = now_ms();
    let key = format!("setup:{ip}");
    if let Err(e) = st.throttle.check(&[&key], now) { return (StatusCode::FORBIDDEN, form(Some(&e.to_string()))).into_response(); }
    let expected = st.setup_code.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let Some(expected) = expected else {
        return form(Some("No setup code is active. Restart sdrtrunk-rust to get one.")).into_response();
    };
    if f.code.trim().to_uppercase() != expected {
        st.throttle.failed(&[&key], now);
        return (StatusCode::FORBIDDEN, form(Some("Wrong setup code. It is printed in the receiver log."))).into_response();
    }
    if f.password != f.password2 { return form(Some("The passwords do not match.")).into_response(); }
    let user = match st.db.create_user(f.username.trim(), &f.password, Role::Owner, now) {
        Ok(u) => u,
        Err(e) => return form(Some(&e.to_string())).into_response(),
    };
    *st.setup_code.lock().unwrap_or_else(|p| p.into_inner()) = None;
    st.db.audit(now, &user.username, "created owner account");
    let token = st.db.create_session(user.id, now);
    ([(SET_COOKIE, st.session_cookie(&token))], Redirect::to("/")).into_response()
}
