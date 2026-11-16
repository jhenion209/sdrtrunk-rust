//! Tuners: every SDR (connected now, in use, or configured before), its mode, what it is doing, and its settings.
//! Settings belong to the device (keyed by serial), like sdrtrunk's tuner configuration.
use crate::app::{check_csrf, AppState, Authed};
use crate::auth::Session;
use crate::db::{FieldError, TunerConfig, TunerMode};
use crate::pages::layout::{field_error, layout, layout_with, notice, Head};
use crate::pages::settings::{format_mhz, parse_mhz, Saved};
use crate::receiver::{now_ms, Status};
use axum::extract::{Path, Query, State};
use crate::pages::live_view;
use axum::response::sse::{Event, Sse};
use futures_core::Stream;
use std::convert::Infallible;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use maud::{html, Markup};
use serde::Deserialize;
use sdrtrunk_source::tuner_label;

fn is_rtl(id: &str) -> bool { id.starts_with("rtl:") }

fn mhz(hz: u64) -> String { format!("{:.4}", hz as f64 / 1e6) }

/// "851.8860–854.2380 MHz (2.4 MS/s)": the band a tuned SDR receives usefully (its weak edges left out).
pub(crate) fn window(s: &Status) -> Option<String> {
    let center = s.center_hz.filter(|_| s.rate_hz > 0)?;
    let half = if s.usable_hz > 0 { s.usable_hz } else { s.rate_hz } as u64 / 2;
    Some(format!("{}–{} MHz ({} MS/s)", mhz(center.saturating_sub(half)), mhz(center + half), s.rate_hz as f64 / 1e6))
}

/// "853.0000 MHz (locked)": where the tuner sits.
pub(crate) fn tuned_to(s: &Status) -> Option<String> {
    s.center_hz.map(|c| format!("{} MHz{}", mhz(c), if s.locked { " (locked)" } else { "" }))
}

/// One row of the list: the tuner, its settings, whether it is connected, and what uses it (a system, or Setup mode).
pub(crate) struct Row { pub id: String, pub config: TunerConfig, pub connected: bool, pub user: Option<(String, Status)> }

pub(crate) fn rows(st: &AppState) -> Vec<Row> {
    let devices = st.receivers.devices();
    let configured = st.db.tuners();
    let systems = st.db.systems();
    let users = st.receivers.statuses();
    let mut ids: Vec<String> = devices.iter().cloned().chain(configured.iter().map(|(id, _)| id.clone())).collect();
    ids.sort();
    ids.dedup();
    ids.into_iter().map(|id| {
        let config = configured.iter().find(|(t, _)| *t == id).map(|(_, c)| c.clone()).unwrap_or_default();
        let user = users.iter().find(|(_, s)| s.tuner.as_deref() == Some(id.as_str()))
            .map(|(sys, s)| (systems.iter().find(|x| x.id == *sys).map(|x| x.label()).unwrap_or_default(), s.clone()))
            .or_else(|| st.receivers.setup_status(&id).map(|s| ("Setup mode".to_string(), s)));
        Row { connected: devices.contains(&id), id, config, user }
    }).collect()
}

fn mode_text(m: TunerMode) -> &'static str { match m { TunerMode::Live => "Live", TunerMode::Setup => "Setup", TunerMode::Disabled => "Disabled" } }

/// "-2.912 ppm (automatic)": the correction in use (Automatic PPM moves it while a system runs) or the saved one.
pub(crate) fn correction_text(r: &Row) -> String {
    let ppm = r.user.as_ref().map_or(r.config.ppm, |(_, s)| s.ppm);
    format!("{ppm:.3} ppm{}", if r.config.auto_ppm { " (automatic)" } else { "" })
}

fn gain_text(id: &str, c: &TunerConfig) -> String {
    if is_rtl(id) { c.rtl_gain_db.map_or("automatic".into(), |g| format!("{g} dB")) } else { format!("{} of 21", c.gain) }
}

pub async fn list(State(st): State<AppState>, Authed(s, _): Authed, Query(q): Query<Saved>) -> Markup {
    layout("Tuners", Some(&s.user), &s.csrf, html! {
        (notice(None, q.saved.is_some().then_some("Saved. The system using that tuner restarted.")))
        p.muted { "Each SDR keeps its own frequency correction, gain and sample rate. Systems choose an SDR on the "
            a href="/systems" { "Systems" } " page, or take any free one (Automatic)." }
        div data-live="/tuners/events" { (tuners_table(&st)) }
    })
}

/// The tuners table, redrawn every second while the Tuners page is open.
pub async fn events(State(st): State<AppState>, Authed(_, token): Authed) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    crate::pages::live_html(&st, token, |st| tuners_table(st).into_string())
}

/// Every tuner with what it is doing now.
fn tuners_table(st: &AppState) -> Markup {
    let rows = rows(st);
    html! {
        @if rows.is_empty() { p.muted { "No SDR found. Plug in an Airspy or RTL-SDR." } } @else {
            table {
                thead { tr { th { "Tuner" } th { "Mode" } th { "Used by" } th { "Tuned to" } th { "Receiving" } th { "Correction" } th { "Gain" }
                    th { "Dropped samples" } th {} } }
                tbody { @for r in &rows { tr {
                    td { (tuner_label(&r.id)) @if !r.connected { br; span.muted { "not connected" } } }
                    td { @if r.config.mode == TunerMode::Disabled { span.muted { "Disabled" } } @else { (mode_text(r.config.mode)) } }
                    td { @match &r.user {
                        Some((label, status)) => { (label) @if status.state != "running" { br; span.muted { (status.state) } } }
                        None => { span.muted { "idle" } }
                    } }
                    td { @if let Some((_, status)) = &r.user { (tuned_to(status).unwrap_or_default()) } }
                    td { @if let Some((_, status)) = &r.user { (window(status).unwrap_or_default()) } }
                    td { (correction_text(r)) }
                    td { (gain_text(&r.id, &r.config)) }
                    td { @if let Some((_, status)) = &r.user { (status.dropped) } }
                    td.row-actions { a.button.quiet href={ "/tuners/" (r.id) } { "Settings" } @if r.user.is_some() { a.button.quiet href={ "/tuners/" (r.id) "/spectrum" } { "Spectrum" } } }
                } } }
            }
        }
    }
}

/// Text as typed, so a rejected form comes back as entered.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct TunerForm {
    csrf: String, mode: String, center_mhz: String, min_mhz: String, max_mhz: String, ppm: String, auto_ppm: String, gain: String,
    rate_hz: String, rtl_gain_db: String,
}

/// Blank is None; anything else must be MHz.
fn optional_mhz(s: &str) -> Result<Option<u64>, ()> { if s.trim().is_empty() { Ok(None) } else { parse_mhz(s).map(Some).ok_or(()) } }

impl TunerForm {
    fn from(c: &TunerConfig) -> TunerForm {
        let text = |f: Option<u64>| f.map(format_mhz).unwrap_or_default();
        TunerForm {
            csrf: String::new(),
            mode: c.mode.as_str().into(),
            center_mhz: text(c.center_hz),
            min_mhz: text(c.min_hz),
            max_mhz: text(c.max_hz),
            ppm: c.ppm.to_string(),
            auto_ppm: if c.auto_ppm { "on" } else { "" }.into(),
            gain: c.gain.to_string(),
            rate_hz: c.rate_hz.to_string(),
            rtl_gain_db: c.rtl_gain_db.map(|g| g.to_string()).unwrap_or_default(),
        }
    }

    /// Fields the form for this kind of tuner does not show keep their stored values.
    fn to_config(&self, id: &str, stored: &TunerConfig) -> Result<TunerConfig, Vec<FieldError>> {
        let mut errors = Vec::new();
        let mut bad = |field, message: &str| errors.push(FieldError { field, message: message.into() });
        let mut c = stored.clone();
        c.mode = match self.mode.as_str() { "setup" => TunerMode::Setup, "disabled" => TunerMode::Disabled, _ => TunerMode::Live };
        match optional_mhz(&self.center_mhz) { Ok(v) => c.center_hz = v, Err(_) => bad("center_hz", "Enter MHz such as 853.0, or leave blank.") }
        match (optional_mhz(&self.min_mhz), optional_mhz(&self.max_mhz)) {
            (Ok(lo), Ok(hi)) => (c.min_hz, c.max_hz) = (lo, hi),
            _ => bad("limits", "Enter MHz such as 851.0, or leave blank."),
        }
        match self.ppm.trim().parse() { Ok(v) => c.ppm = v, Err(_) => bad("ppm", "Enter a number such as -4 or 2.5.") }
        c.auto_ppm = self.auto_ppm == "on";
        if is_rtl(id) {
            let g = self.rtl_gain_db.trim();
            c.rtl_gain_db = if g.is_empty() { None } else {
                match g.parse() { Ok(v) => Some(v), Err(_) => { bad("rtl_gain_db", "Enter dB such as 32.8, or leave blank for automatic."); None } }
            };
        } else {
            match self.gain.trim().parse() { Ok(v) => c.gain = v, Err(_) => bad("gain", "Gain must be 0 to 21.") }
            match self.rate_hz.trim().parse() { Ok(v) => c.rate_hz = v, Err(_) => bad("rate_hz", "Sample rate must be 3 or 6 MS/s.") }
        }
        if let Err(more) = c.validate() { for e in more { if !errors.iter().any(|x| x.field == e.field) { errors.push(e); } } }
        if errors.is_empty() { Ok(c) } else { Err(errors) }
    }
}

fn form_view(st: &AppState, s: &Session, id: &str, f: &TunerForm, errors: &[FieldError]) -> Markup {
    let row = rows(st).into_iter().find(|r| r.id == id);
    let in_use = row.as_ref().is_some_and(|r| r.user.is_some());
    let head = Head {
        subtitle: Some(html! { "Mode, tuning, frequency correction and gain of this SDR." }),
        actions: in_use.then(|| html! { a.button href={ "/tuners/" (id) "/spectrum" } { "Spectrum" } }),
    };
    layout_with(&tuner_label(id), head, Some(&s.user), &s.csrf, html! {
        (field_error(errors, "form"))
        @if let Some(r) = &row {
            dl.facts {
                dt { "Connected" } dd { @if r.connected { "yes" } @else { "no" } }
                dt { "Used by" } dd { @match &r.user { Some((label, status)) => { (label) " (" (status.state) ")" } None => { "nothing" } } }
                @if let Some((_, status)) = &r.user {
                    @if let Some(t) = tuned_to(status) { dt { "Tuned to" } dd { (t) } }
                    @if let Some(w) = window(status) { dt { "Receiving" } dd { (w) } }
                    dt { "Dropped samples" } dd { (status.dropped) }
                    dt { "Frequency error" } dd { (crate::pages::signal::error_text(status.freq_error_hz, status.control_hz)) }
                    dt { "Correction in use" } dd { (correction_text(r)) }
                }
            }
        }
        form method="post" action={ "/tuners/" (id) } {
            input type="hidden" name="csrf" value=(s.csrf);
            fieldset {
                legend { "Mode" }
                label.check { input type="radio" name="mode" value="live" checked[f.mode == "live" || f.mode.is_empty()]; "Live: systems may use this SDR" }
                label.check { input type="radio" name="mode" value="setup" checked[f.mode == "setup"];
                    "Setup: runs at the center below so you can watch its spectrum; no system uses it" }
                label.check { input type="radio" name="mode" value="disabled" checked[f.mode == "disabled"];
                    "Disabled: never opened, not even by Automatic" }
            }
            fieldset {
                legend { "Tuning" }
                label { "Lock center (MHz, blank for automatic)" input name="center_mhz" inputmode="decimal" value=(f.center_mhz); }
                p.muted { "Automatic places the receiving window over the control channel and the voice frequencies the system has "
                    "used, and moves it when a call falls outside, as long as the control channel and calls in progress stay "
                    "inside. A locked center never moves: calls outside its window are not recorded." }
                (field_error(errors, "center_hz"))
                label { "Minimum frequency (MHz)" input name="min_mhz" inputmode="decimal" value=(f.min_mhz); }
                label { "Maximum frequency (MHz)" input name="max_mhz" inputmode="decimal" value=(f.max_mhz); }
                p.muted { "Frequency limits: the center and every channel stay between these. Blank: no limit." }
                (field_error(errors, "limits"))
            }
            label { "Frequency correction (ppm)" input name="ppm" inputmode="decimal" value=(f.ppm); }
            p.muted { "Corrects this SDR's clock error. The Airspy on the Pi reads about -4 ppm; cheap RTL-SDRs without a TCXO can be "
                "tens of ppm off and drift as they warm up." }
            (field_error(errors, "ppm"))
            label.check { input type="checkbox" name="auto_ppm" value="on" checked[f.auto_ppm == "on"];
                "Automatic PPM: follow the measured frequency error" }
            p.muted { "While a control channel decodes, the correction moves toward the measured error, at most 3 ppm from "
                "the value above, and is saved as it moves. For a large error, turn this off, let the SDR warm up, set the "
                "correction by hand until the system decodes, then turn it back on." }
            @if is_rtl(id) {
                label { "Gain (dB, blank for automatic)" input name="rtl_gain_db" inputmode="decimal" value=(f.rtl_gain_db); }
                p.muted { "The dongle uses the nearest gain step its tuner has. RTL-SDRs always sample at 2.4 MS/s." }
                (field_error(errors, "rtl_gain_db"))
            } @else {
                label { "Gain (0 to 21)" input name="gain" type="number" min="0" max="21" value=(f.gain); }
                (field_error(errors, "gain"))
                label { "Sample rate"
                    select name="rate_hz" {
                        option value="6000000" selected[f.rate_hz == "6000000"] { "6 MS/s" }
                        option value="3000000" selected[f.rate_hz == "3000000"] { "3 MS/s" }
                    }
                }
                (field_error(errors, "rate_hz"))
            }
            div.form-actions { button { "Save and restart the system using it" } a.button.quiet href="/tuners" { "Cancel" } }
        }
    })
}

/// A tuner id in its stored form ("735863DC-2F67AFE3" or "rtl:00000001"); anything else is not a tuner.
fn valid(id: &str) -> bool { sdrtrunk_source::normalize_tuner(id).as_deref() == Some(id) }

pub async fn edit_page(State(st): State<AppState>, Authed(s, _): Authed, Path(id): Path<String>) -> Response {
    if !valid(&id) { return (StatusCode::NOT_FOUND, "No such tuner.").into_response(); }
    form_view(&st, &s, &id, &TunerForm::from(&st.db.tuner(&id)), &[]).into_response()
}

pub async fn save(State(st): State<AppState>, Authed(s, _): Authed, Path(id): Path<String>, Form(f): Form<TunerForm>) -> Response {
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    if !valid(&id) { return (StatusCode::NOT_FOUND, "No such tuner.").into_response(); }
    let c = match f.to_config(&id, &st.db.tuner(&id)) { Ok(c) => c, Err(errors) => return form_view(&st, &s, &id, &f, &errors).into_response() };
    if let Err(e) = st.db.save_tuner(&id, &c) {
        return form_view(&st, &s, &id, &f, &[FieldError { field: "form", message: format!("Could not save: {e}") }]).into_response();
    }
    // ponytail: restarts join decoder threads (up to ~200 ms each) on this request; fine for a settings form.
    st.receivers.apply_tuner(&st.db, &id);
    st.db.audit(now_ms(), &s.user.username, &format!("changed tuner {}", tuner_label(&id)));
    Redirect::to("/tuners?saved=1").into_response()
}

// ---------- spectrum ----------

pub async fn spectrum_page(State(st): State<AppState>, Authed(s, _): Authed, Path(id): Path<String>) -> Response {
    if !valid(&id) { return (StatusCode::NOT_FOUND, "No such tuner.").into_response(); }
    let user = st.receivers.on_tuner(&id).map(|r| format!("Used by {}.", r.system.label()))
        .or_else(|| st.receivers.setup_status(&id).map(|s| format!("Setup mode at {}.", tuned_to(&s).unwrap_or_default())));
    let head = Head {
        subtitle: Some(html! { (user.as_deref().unwrap_or("Not in use.")) }),
        actions: Some(html! { a.button.quiet href={ "/tuners/" (id) } { "Settings" } }),
    };
    layout_with(&format!("{} spectrum", tuner_label(&id)), head, Some(&s.user), &s.csrf, html! {
        p.muted { @if user.is_none() { "The display starts when a system uses this tuner, or in Setup mode. " }
            "Green: control channel. Orange: voice channels being followed. Red: encrypted voice channels. Shaded: the band "
            "edges, not used for channels. "
            "A signal sitting beside its marker shows the tuner's frequency error (set the correction on the "
            a href={ "/tuners/" (id) } { "tuner's settings" } ")." }
        div.spectrum-controls {
            button #hold type="button" { "Hold" }
            label.check { input #auto-range type="checkbox" checked; "Automatic range" }
            label { "Floor (dB)" input #floor type="number" step="5" value="-100"; }
            label { "Ceiling (dB)" input #ceiling type="number" step="5" value="-20"; }
            span #readout .muted {}
        }
        p #spectrum-status .muted {}
        canvas #spectrum .spectrum width="1024" height="240" data-src={ "/tuners/" (id) "/spectrum/events" } {}
        canvas #waterfall .spectrum width="1024" height="320" {}
    }).into_response()
}

/// `frame` events (about 10 a second) while a system uses the tuner or it is in Setup mode, `idle` once a second otherwise.
pub async fn spectrum_events(State(st): State<AppState>, Authed(_, token): Authed, Path(id): Path<String>)
    -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    if !valid(&id) { return Err(StatusCode::NOT_FOUND); }
    Ok(live_view(&st, token, "frame", "No system is using this tuner.", move |st| st.receivers.tuner_view(&id), |v, until| {
        v.watch_until_ms = until;
        v.frame.as_ref().and_then(|f| Some((f.seq, serde_json::to_string(f).ok()?)))
    }))
}
