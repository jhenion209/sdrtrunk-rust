//! Recordings page, plus the MHz helpers the Systems page uses.
use crate::app::{check_csrf, AppState, Authed};
use crate::auth::Session;
use crate::db::{FieldError, Settings};
use crate::pages::layout::{field_error, layout, notice};
use crate::receiver::now_ms;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use maud::{html, Markup};
use serde::Deserialize;

#[derive(Deserialize, Default)]
pub struct Saved { pub saved: Option<u8> }

/// "858.9625" -> 858_962_500. Up to 6 decimals, no signs or exponents.
pub fn parse_mhz(s: &str) -> Option<u64> {
    let s = s.trim();
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
    if int.is_empty() || int.len() > 5 || frac.len() > 6 || !digits(int) || !digits(frac) { return None; }
    Some(int.parse::<u64>().ok()? * 1_000_000 + format!("{frac:0<6}").parse::<u64>().ok()?)
}

/// 858_962_500 -> "858.9625"
pub fn format_mhz(hz: u64) -> String {
    let s = format!("{}.{:06}", hz / 1_000_000, hz % 1_000_000);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

// ---------- Recordings ----------

#[derive(Deserialize)]
pub struct RecordingsForm { #[serde(default)] csrf: String, recording_dir: String }

fn recordings_view(s: &Session, f: &RecordingsForm, errors: &[FieldError], saved: bool) -> Markup {
    layout("Recordings", Some(&s.user), &s.csrf, html! {
        (notice(None, saved.then_some("Saved. Every system restarted.")))
        (field_error(errors, "form"))
        p.muted { "Each clear call from every system is saved here as an MP3 for ScannerPulse, named and tagged like sdrtrunk's. "
            "File names use each system's System, Site and Channel names (set on the " a href="/systems" { "Systems" } " page)." }
        form method="post" action="/recordings" {
            input type="hidden" name="csrf" value=(s.csrf);
            label { "Folder on this computer" input name="recording_dir" value=(f.recording_dir); }
            (field_error(errors, "recording_dir"))
            button { "Save and restart all systems" }
        }
    })
}

pub async fn recordings_page(State(st): State<AppState>, Authed(s, _): Authed, Query(q): Query<Saved>) -> Markup {
    recordings_view(&s, &RecordingsForm { csrf: String::new(), recording_dir: st.db.settings().recording_dir }, &[], q.saved.is_some())
}

pub async fn recordings_submit(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<RecordingsForm>) -> Response {
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    let new = Settings { recording_dir: f.recording_dir.trim().to_string() };
    if let Err(errors) = new.validate() { return recordings_view(&s, &f, &errors, false).into_response(); }
    if let Err(e) = st.db.save_settings(&new) {
        return recordings_view(&s, &f, &[FieldError { field: "form", message: format!("Could not save: {e}") }], false).into_response();
    }
    // ponytail: restarts join every decoder thread (up to ~200 ms each) on this request; fine for a settings form.
    st.receivers.restart_all(&st.db);
    st.db.audit(now_ms(), &s.user.username, "changed recordings directory");
    Redirect::to("/recordings?saved=1").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mhz_parse_and_format() {
        assert_eq!(parse_mhz("858.9625"), Some(858_962_500));
        assert_eq!(parse_mhz(" 860.5 "), Some(860_500_000));
        assert_eq!(parse_mhz("851"), Some(851_000_000));
        for bad in ["", "abc", "-5", "1e9", "858.1234567", "8.5.1", "999999999"] { assert_eq!(parse_mhz(bad), None, "{bad}"); }
        assert_eq!(format_mhz(858_962_500), "858.9625");
        assert_eq!(format_mhz(851_000_000), "851");
    }
}
