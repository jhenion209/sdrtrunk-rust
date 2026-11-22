//! Systems: add, edit and delete the trunked systems sdrtrunk-rust follows. Saving one restarts only that system.
use crate::app::{check_csrf, require, AppState, Authed};
use crate::auth::Function;
use crate::auth::Session;
use crate::db::{FieldError, System};
use crate::pages::layout::{field_error, layout, layout_with, notice, Head, read_only};
use crate::pages::settings::{format_mhz, parse_mhz, Saved};
use crate::receiver::now_ms;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use maud::{html, Markup};
use serde::Deserialize;

/// Every field is text as typed, so a rejected form comes back exactly as entered.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SystemForm {
    csrf: String,
    system_name: String,
    site_name: String,
    channel_name: String,
    primary: String,
    backups: String,
    failover_ms: String,
    modulation: String,
    tuner: String,
    /// Alias list name, or "" for the list named after the system.
    alias_list: String,
    /// Checkbox: present ("on") when ticked.
    enabled: Option<String>,
}

impl SystemForm {
    fn from(s: &System) -> SystemForm {
        SystemForm {
            csrf: String::new(),
            system_name: s.system_name.clone(),
            site_name: s.site_name.clone(),
            channel_name: s.channel_name.clone(),
            primary: s.control_hz.first().map(|&h| format_mhz(h)).unwrap_or_default(),
            backups: s.control_hz.iter().skip(1).map(|&h| format_mhz(h)).collect::<Vec<_>>().join("\n"),
            failover_ms: s.failover_ms.to_string(),
            modulation: s.modulation.clone(),
            tuner: s.tuner.clone(),
            alias_list: s.alias_list.clone(),
            enabled: s.enabled.then(|| "on".into()),
        }
    }

    fn to_system(&self, id: i64) -> Result<System, Vec<FieldError>> {
        let mut errors = Vec::new();
        let mut bad = |field, message: &str| errors.push(FieldError { field, message: message.into() });
        let lines: Vec<&str> = std::iter::once(self.primary.as_str()).chain(self.backups.lines()).map(str::trim).filter(|l| !l.is_empty()).collect();
        let control_hz = lines.iter().map(|l| parse_mhz(l)).collect::<Option<Vec<u64>>>().unwrap_or_else(|| {
            bad("control_hz", "Enter frequencies in MHz, for example 858.9625.");
            Vec::new()
        });
        if self.primary.trim().is_empty() { bad("control_hz", "Enter the control channel frequency."); }
        let failover_ms = self.failover_ms.trim().parse().unwrap_or_else(|_| { bad("failover_ms", "Use 200 to 60000 ms."); 0 });
        // Any serial form is accepted and stored the way sdrtrunk writes it.
        let tuner = self.tuner.trim();
        let tuner = sdrtrunk_source::normalize_tuner(tuner).unwrap_or_else(|| tuner.to_string());
        let s = System {
            id, enabled: self.enabled.is_some(), tuner, control_hz, failover_ms, modulation: self.modulation.clone(),
            system_name: self.system_name.trim().into(), site_name: self.site_name.trim().into(), channel_name: self.channel_name.trim().into(),
            alias_list: self.alias_list.trim().into(),
        };
        // Report both the parse errors and the range checks, without repeating a field.
        if let Err(more) = s.validate() { for e in more { if !errors.iter().any(|x| x.field == e.field) { errors.push(e); } } }
        if errors.is_empty() { Ok(s) } else { Err(errors) }
    }
}

fn state_cell(st: &AppState, s: &System) -> Markup {
    match (s.enabled, st.receivers.status(s.id)) {
        (false, _) => html! { span.muted { "off" } },
        (true, Some(status)) if status.state == "error" => html! { span.error { "error: " (status.error.unwrap_or_default()) } },
        (true, Some(status)) => html! { (status.state) @if let Some(t) = status.tuner { span.muted { " on " (sdrtrunk_source::tuner_label(&t)) } } },
        (true, None) => html! { span.muted { "not started" } },
    }
}

pub async fn list(State(st): State<AppState>, Authed(s, _): Authed, Query(q): Query<Saved>) -> Markup {
    let systems = st.db.systems();
    let head = Head {
        subtitle: Some(html! { "Each system is one trunked site: its control channel, SDR and recording names." }),
        actions: st.db.may(&s.user, Function::SystemsAdd).then(|| html! { a.button href="/systems/new" { "Add a system" } }),
    };
    layout_with("Systems", head, Some(&s.user), &s.csrf, html! {
        (notice(None, q.saved.is_some().then_some("Saved. That system restarted.")))
        @if systems.is_empty() { p.muted { "No systems yet. Add one to start receiving." } } @else {
            table {
                thead { tr { th { "System / Channel" } th { "Site" } th { "Control channel" } th { "Modulation" } th { "Tuner" } th { "State" } th {} } }
                tbody { @for sys in &systems { tr {
                    td { (sys.label()) br; span.muted { "alias list " (sys.list_name()) } }
                    td { (sys.site_name) }
                    td { (sys.control_hz.first().map(|&h| format_mhz(h)).unwrap_or_default()) " MHz"
                        @if sys.control_hz.len() > 1 { span.muted { " +" (sys.control_hz.len() - 1) " backup" } } }
                    td { (if sys.modulation == "c4fm" { "C4FM" } else { "LSM" }) }
                    td { @if sys.tuner.is_empty() { "Automatic" } @else { (sdrtrunk_source::tuner_label(&sys.tuner)) } }
                    td { (state_cell(&st, sys)) }
                    td.row-actions { a.button.quiet href={ "/systems/" (sys.id) "/signal" } { "Signal" } a.button.quiet href={ "/systems/" (sys.id) } { "Settings" } }
                } } }
            }
        }
    })
}

fn form_view(st: &AppState, s: &Session, id: Option<i64>, f: &SystemForm, errors: &[FieldError]) -> Markup {
    let devices = st.receivers.devices();
    let lists = st.db.alias_lists();
    let dash = |n: &str| n.trim().replace(' ', "-");
    let example = format!("20261113_210324_{}_{}_{}__TO_23_FROM_703313.mp3", dash(&f.system_name), dash(&f.site_name), dash(&f.channel_name));
    let action = id.map_or("/systems/new".to_string(), |id| format!("/systems/{id}"));
    let title = if id.is_some() { "Edit system" } else { "Add system" };
    let need = if id.is_some() { Function::SystemsEdit } else { Function::SystemsAdd };
    let can = st.db.may(&s.user, need);
    if id.is_none() && !can { return layout(title, Some(&s.user), &s.csrf, read_only(need)); }
    layout(title, Some(&s.user), &s.csrf, html! {
        (field_error(errors, "form"))
        @if !can { (read_only(need)) }
        form method="post" action=(action) {
            input type="hidden" name="csrf" value=(s.csrf);
            fieldset.lock disabled[!can] {
            label.check { input type="checkbox" name="enabled" checked[f.enabled.is_some()]; "Receive this system" }
            h2 { "Names" }
            label { "System name" input name="system_name" value=(f.system_name) required; }
            label { "Site name" input name="site_name" value=(f.site_name) required; }
            label { "Channel name" input name="channel_name" value=(f.channel_name) required; }
            p.muted { "Use the names from sdrtrunk's alias list so ScannerPulse files calls the same way. Example file: " code { (example) } }
            (field_error(errors, "names"))
            label { "Talkgroup alias list"
                select name="alias_list" {
                    option value="" selected[f.alias_list.is_empty()] { "The list named after the system" }
                    @for l in &lists { option value=(l) selected[&f.alias_list == l] { (l) } }
                }
            }
            p.muted { "Names and Record settings for this system's talkgroups. Lists are edited and imported on the "
                a href="/talkgroups" { "Talkgroups" } " page; several systems may share one." }
            (field_error(errors, "alias_list"))
            h2 { "Control channel" }
            label { "Control channel (MHz)" input name="primary" inputmode="decimal" value=(f.primary); }
            (field_error(errors, "control_hz"))
            label { "Backup control channels (MHz, one per line, tried in order)" textarea name="backups" rows="3" { (f.backups) } }
            label { "Switch to the next channel after (ms without a control message)"
                input name="failover_ms" type="number" min="200" max="60000" value=(f.failover_ms); }
            (field_error(errors, "failover_ms"))
            label { "Modulation"
                select name="modulation" {
                    option value="lsm" selected[f.modulation == "lsm"] { "LSM / CQPSK (simulcast sites)" }
                    option value="c4fm" selected[f.modulation == "c4fm"] { "C4FM" }
                }
            }
            p.muted { "Use the same setting as sdrtrunk. Simulcast systems need LSM." }
            (field_error(errors, "modulation"))
            h2 { "Tuner" }
            label { "SDR"
                select name="tuner" {
                    option value="" selected[f.tuner.is_empty()] { "Automatic (any free SDR)" }
                    @for d in &devices { option value=(d) selected[&f.tuner == d] { (sdrtrunk_source::tuner_label(d)) } }
                    @if !f.tuner.is_empty() && !devices.contains(&f.tuner) {
                        option value=(f.tuner) selected { (sdrtrunk_source::tuner_label(&f.tuner)) " (not connected)" } }
                }
            }
            p.muted { "Pick one to tie this system to that SDR, like sdrtrunk's preferred tuner. Automatic takes any SDR no other "
                "system is using or tied to. An RTL-SDR covers about 2.2 MHz around the control channel; calls on voice "
                "channels outside that are not recorded." }
            (field_error(errors, "tuner"))
            p.muted { "Gain, frequency correction and sample rate belong to the SDR: set them on the " a href="/tuners" { "Tuners" } " page." }
            }
            @if can { button { "Save and restart this system" } " " } a href="/systems" { "Cancel" }
        }
        @if let Some(id) = id.filter(|_| st.db.may(&s.user, Function::SystemsDelete)) {
            h2 { "Delete" }
            form method="post" action={ "/systems/" (id) "/delete" } {
                input type="hidden" name="csrf" value=(s.csrf);
                p.muted { "Stops the system and removes it. Its talkgroup list is kept." }
                button.danger { "Delete this system" }
            }
        }
    })
}

pub async fn new_page(State(st): State<AppState>, Authed(s, _): Authed) -> Markup {
    form_view(&st, &s, None, &SystemForm::from(&System::default()), &[])
}

pub async fn edit_page(State(st): State<AppState>, Authed(s, _): Authed, Path(id): Path<i64>) -> Response {
    match st.db.system(id) {
        Some(sys) => form_view(&st, &s, Some(id), &SystemForm::from(&sys), &[]).into_response(),
        None => (StatusCode::NOT_FOUND, "No such system.").into_response(),
    }
}

fn store(st: &AppState, s: &Session, id: Option<i64>, f: &SystemForm) -> Response {
    let sys = match f.to_system(id.unwrap_or(0)) { Ok(sys) => sys, Err(errors) => return form_view(st, s, id, f, &errors).into_response() };
    let saved = match st.db.save_system(&sys) {
        Ok(saved) => saved,
        Err(e) => return form_view(st, s, id, f, &[FieldError { field: "form", message: format!("Could not save: {e}") }]).into_response(),
    };
    // ponytail: the restart joins the old decoder thread (up to ~200 ms) on this request; fine for a settings form.
    st.receivers.apply(&st.db, saved);
    st.db.audit(now_ms(), &s.user.username, &format!("{} system {}", if id.is_some() { "changed" } else { "added" }, sys.label()));
    Redirect::to("/systems?saved=1").into_response()
}

pub async fn create(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<SystemForm>) -> Response {
    if let Some(r) = require(&st, &s, Function::SystemsAdd) { return r; }
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    store(&st, &s, None, &f)
}

pub async fn save(State(st): State<AppState>, Authed(s, _): Authed, Path(id): Path<i64>, Form(f): Form<SystemForm>) -> Response {
    if let Some(r) = require(&st, &s, Function::SystemsEdit) { return r; }
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    if st.db.system(id).is_none() { return (StatusCode::NOT_FOUND, "No such system.").into_response(); }
    store(&st, &s, Some(id), &f)
}

#[derive(Deserialize)]
pub struct CsrfOnly { #[serde(default)] csrf: String }

pub async fn delete(State(st): State<AppState>, Authed(s, _): Authed, Path(id): Path<i64>, Form(f): Form<CsrfOnly>) -> Response {
    if let Some(r) = require(&st, &s, Function::SystemsDelete) { return r; }
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    let Some(sys) = st.db.system(id) else { return (StatusCode::NOT_FOUND, "No such system.").into_response() };
    if let Err(e) = st.db.delete_system(id) {
        return form_view(&st, &s, Some(id), &SystemForm::from(&sys), &[FieldError { field: "form", message: format!("Could not delete: {e}") }]).into_response();
    }
    st.receivers.apply(&st.db, id);
    st.db.audit(now_ms(), &s.user.username, &format!("deleted system {}", sys.label()));
    Redirect::to("/systems").into_response()
}
