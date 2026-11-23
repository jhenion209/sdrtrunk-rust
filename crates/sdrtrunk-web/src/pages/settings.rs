//! Recordings page, plus the MHz helpers the Systems page uses.
use crate::app::{check_csrf, require, AppState, Authed};
use crate::auth::Function;
use crate::auth::Session;
use crate::db::{FieldError, Settings};
use crate::pages::layout::{field_error, layout, notice, read_only};
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
pub struct RecordingsForm {
    #[serde(default)] csrf: String,
    recording_dir: String,
    #[serde(default)] event_log_dir: String,
    #[serde(default)] event_log_days: String,
}

/// `can`: the signed-in admin may change these settings (else the form is shown locked).
fn recordings_view(s: &Session, f: &RecordingsForm, errors: &[FieldError], saved: bool, can: bool) -> Markup {
    layout("Recordings", Some(&s.user), &s.csrf, html! {
        (notice(None, saved.then_some("Saved. Every system restarted.")))
        (field_error(errors, "form"))
        p.muted { "Each clear call from every system is saved here as an MP3 for ScannerPulse, named and tagged like sdrtrunk's. "
            "File names use each system's System, Site and Channel names (set on the " a href="/systems" { "Systems" } " page)." }
        @if !can { (read_only(Function::RecordingsEdit)) }
        form method="post" action="/recordings" {
            input type="hidden" name="csrf" value=(s.csrf);
            fieldset.lock disabled[!can] {
            label { "Folder on this computer" input name="recording_dir" value=(f.recording_dir); }
            (field_error(errors, "recording_dir"))
            p.muted { "Every call each system hears is also logged, one line per call, to a daily CSV file in sdrtrunk's call event "
                "format. Search them on the " a href="/event-log" { "Event Log" } " page." }
            label { "Call event log folder (empty: off)" input name="event_log_dir" value=(f.event_log_dir); }
            (field_error(errors, "event_log_dir"))
            label { "Keep call event logs for (days)" input type="number" name="event_log_days" min="1" max="3650" value=(f.event_log_days); }
            (field_error(errors, "event_log_days"))
            }
            @if can { button { "Save and restart all systems" } }
        }
    })
}

pub async fn recordings_page(State(st): State<AppState>, Authed(s, _): Authed, Query(q): Query<Saved>) -> Markup {
    let cur = st.db.settings();
    let f = RecordingsForm { csrf: String::new(), recording_dir: cur.recording_dir, event_log_dir: cur.event_log_dir,
        event_log_days: cur.event_log_days.to_string() };
    recordings_view(&s, &f, &[], q.saved.is_some(), st.db.may(&s.user, Function::RecordingsEdit))
}

pub async fn recordings_submit(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<RecordingsForm>) -> Response {
    if let Some(r) = require(&st, &s, Function::RecordingsEdit) { return r; }
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    // An unparsable number becomes 0, which validate rejects with the days message.
    let new = Settings { recording_dir: f.recording_dir.trim().to_string(), event_log_dir: f.event_log_dir.trim().to_string(),
        event_log_days: f.event_log_days.trim().parse().unwrap_or(0) };
    if let Err(errors) = new.validate() { return recordings_view(&s, &f, &errors, false, true).into_response(); }
    let old = st.db.settings();
    if let Err(e) = st.db.save_settings(&new) {
        return recordings_view(&s, &f, &[FieldError { field: "form", message: format!("Could not save: {e}") }], false, true).into_response();
    }
    // ponytail: restarts join every decoder thread (up to ~200 ms each) on this request; fine for a settings form.
    st.receivers.restart_all(&st.db);
    let mut changed = Vec::new();
    if new.recording_dir != old.recording_dir { changed.push(format!("recordings folder to {}", new.recording_dir)); }
    if new.event_log_dir != old.event_log_dir {
        changed.push(if new.event_log_dir.is_empty() { "event log off".into() } else { format!("event log folder to {}", new.event_log_dir) });
    }
    if new.event_log_days != old.event_log_days { changed.push(format!("event log days to {}", new.event_log_days)); }
    if !changed.is_empty() { st.db.audit(now_ms(), &s.user.username, &format!("changed {}", changed.join("; "))); }
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
