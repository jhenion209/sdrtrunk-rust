//! Owner only: Connected hosts, the signed-in sessions (account, address, browser, since when, live pages), each with
//! Sign out; and at the top, where the web UI listens and which certificate it serves.
use crate::app::{check_csrf, cookie_token, AppState, OwnerOnly, Secure};
use crate::auth::{token_hash, Role, Session};
use crate::pages::layout::{layout, notice};
use crate::pages::login::CsrfForm;
use crate::receiver::now_ms;
use axum::extract::{Path, Query, State};
use axum::http::{header::SET_COOKIE, HeaderMap};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use chrono::{Local, TimeZone};
use maud::{html, Markup};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Gone { gone: Option<u8> }

pub async fn page(State(st): State<AppState>, OwnerOnly(s): OwnerOnly, headers: HeaderMap, Query(q): Query<Gone>) -> Markup {
    let mine = cookie_token(&headers).map(|t| token_hash(&t)).unwrap_or_default();
    view(&st, &s, &mine, q.gone.is_some().then_some("That session had already ended."))
}

pub async fn end(State(st): State<AppState>, OwnerOnly(s): OwnerOnly, Secure(secure): Secure, headers: HeaderMap,
                 Path(row): Path<i64>, Form(f): Form<CsrfForm>) -> Response {
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    let Some(ended) = st.db.end_session_by_row(row) else { return Redirect::to("/hosts?gone=1").into_response() };
    st.db.audit(now_ms(), &s.user.username, &format!("ended {}'s session from {}", ended.username, ended.ip));
    let mine = cookie_token(&headers).is_some_and(|t| token_hash(&t) == ended.token_hash);
    if mine { return ([(SET_COOKIE, st.clear_cookie(secure))], Redirect::to("/login")).into_response(); }
    Redirect::to("/hosts").into_response()
}

fn view(st: &AppState, s: &Session, mine: &str, ok: Option<&str>) -> Markup {
    let now = now_ms();
    let rows = st.db.sessions(now);
    let w = &st.web;
    let found = w.certs.as_ref().and_then(|c| crate::cert::found(&c.current()).ok());
    let renewal = w.renewal.lock().unwrap_or_else(|p| p.into_inner()).clone();
    layout("Connected hosts", Some(&s.user), &s.csrf, html! {
        (notice(None, ok))
        section.card {
            h2 { "Web access" }
            p { "HTTPS: " @if let Some(a) = w.https { "https://" (a) } @else { "off" } }
            p { "HTTP: " @if let Some(a) = w.http { "http://" (a) } @else { "off" } }
            @if w.https.is_some() {
                p { "Certificate: " (w.source)
                    @if let Some(f) = &found { ", for " (f.names.join(", ")) ", until " (f.not_after.format("%Y-%m-%d")) } }
                @if w.source == "Let's Encrypt" {
                    p { "Let's Encrypt: "
                        @if let Some(t) = renewal.tried { "last asked " (t.with_timezone(&Local).format("%Y-%m-%d %H:%M")) ", " } @else { "not asked yet" }
                        @if let Some(e) = &renewal.error { "failed: " (e) } @else if renewal.tried.is_some() { "got it" } }
                }
            }
        }
        @if rows.is_empty() { p.muted { "Nobody is signed in." } } @else {
            table {
                thead { tr { th { "Account" } th { "Address" } th { "Browser" } th { "Signed in" } th { "Last active" }
                             th { "Duration" } th { "Live pages" } th {} } }
                tbody { @for r in &rows { tr {
                    td { (r.username) @if r.role == Role::Owner { " (owner)" } @if r.token_hash == mine { " (this one)" } }
                    td { (r.ip) }
                    td title=(r.agent) { (browser(&r.agent)) }
                    td { (local(r.created_at)) }
                    td { (ago(now - r.last_seen, r.last_seen)) }
                    td { (span(now - r.created_at)) }
                    td { (st.live.count(&r.token_hash)) }
                    td { form method="post" action={ "/hosts/" (r.row) "/end" } {
                        input type="hidden" name="csrf" value=(s.csrf);
                        button.danger { "Sign out" }
                    } }
                } } }
            }
        }
    })
}

fn local(ms: i64) -> String {
    Local.timestamp_millis_opt(ms).single().map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

/// "now" under a minute, "4 min ago" under an hour, else the time.
fn ago(ms: i64, at: i64) -> String {
    match ms / 60_000 { 0 => "now".into(), m @ 1..=59 => format!("{m} min ago"), _ => local(at) }
}

/// "15m", "2h 41m".
fn span(ms: i64) -> String {
    let m = ms.max(0) / 60_000;
    if m < 60 { format!("{m}m") } else { format!("{}h {}m", m / 60, m % 60) }
}

/// "Firefox, macOS" from a `User-Agent`; a part not recognised is "Other", and "Other" alone when neither is.
pub fn browser(agent: &str) -> String {
    let name = [("Edg/", "Edge"), ("Firefox/", "Firefox"), ("Chrome/", "Chrome"), ("Safari/", "Safari")]
        .iter().find(|(m, _)| agent.contains(m)).map(|(_, n)| *n);
    let system = [("Android", "Android"), ("iPhone", "iOS"), ("iPad", "iOS"), ("Windows", "Windows"), ("Mac OS X", "macOS"), ("Linux", "Linux")]
        .iter().find(|(m, _)| agent.contains(m)).map(|(_, n)| *n);
    match (name, system) {
        (None, None) => "Other".into(),
        (n, s) => format!("{}, {}", n.unwrap_or("Other"), s.unwrap_or("Other")),
    }
}

#[cfg(test)]
mod tests {
    use super::browser;

    #[test]
    fn browsers_and_systems_are_named() {
        let firefox_mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15.1; rv:140.0) Gecko/20100101 Firefox/140.0";
        let edge_win = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36 Edg/141.0.0.0";
        let chrome_android = "Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Mobile Safari/537.36";
        let safari_ios = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1";
        let chrome_linux = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
        assert_eq!(browser(firefox_mac), "Firefox, macOS");
        assert_eq!(browser(edge_win), "Edge, Windows");
        assert_eq!(browser(chrome_android), "Chrome, Android");
        assert_eq!(browser(safari_ios), "Safari, iOS");
        assert_eq!(browser(chrome_linux), "Chrome, Linux");
        assert_eq!(browser(""), "Other");
        assert_eq!(browser("curl/8.7.1"), "Other");
    }
}
