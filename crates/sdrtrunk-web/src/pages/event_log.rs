//! Event Log page: the call event log files searched with filters, newest first.
use crate::app::{AppState, Authed};
use crate::db::FieldError;
use crate::event_log::{parse_ids, search, Filter, Found, MAX_PAGE};
use crate::pages::layout::{field_error, layout, notice};
use crate::pages::settings::format_mhz;
use axum::extract::{Query, State};
use chrono::{Local, NaiveDateTime};
use maud::{html, Markup};
use serde::{Deserialize, Serialize};

const PER_PAGE: usize = 200;
const INPUT: &str = "%Y-%m-%dT%H:%M";

/// The form's fields as typed: everything is a string, so bad input is answered with a field error, not a 400.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct EventLogQuery {
    #[serde(default)] system: String,
    #[serde(default)] event: String,
    #[serde(default)] encryption: String,
    #[serde(default)] emergency: String,
    #[serde(default)] groups: String,
    #[serde(default)] radios: String,
    #[serde(default)] from: String,
    #[serde(default)] to: String,
    #[serde(default)] page: String,
}

/// The page number asked for; anything unreadable is the first page.
fn page_no(q: &EventLogQuery) -> usize { q.page.trim().parse().unwrap_or(0) }

fn err(field: &'static str, message: &str) -> FieldError { FieldError { field, message: message.into() } }

/// The search the query asks for, or the field errors that stop it.
fn filter(q: &EventLogQuery, keep_days: u32, now: NaiveDateTime) -> Result<Filter, Vec<FieldError>> {
    let mut errors = Vec::new();
    let time = |s: &str, default: NaiveDateTime, field: &'static str, errors: &mut Vec<FieldError>| {
        if s.is_empty() { return default; }
        NaiveDateTime::parse_from_str(s, INPUT).unwrap_or_else(|_| { errors.push(err(field, "Use a date and time.")); default })
    };
    let from = time(&q.from, now.date().and_hms_opt(0, 0, 0).expect("midnight"), "from", &mut errors);
    // To covers its whole minute (the input has no seconds).
    let to = if q.to.is_empty() { now } else { time(&q.to, now, "to", &mut errors) + chrono::Duration::seconds(59) };
    if to < from { errors.push(err("to", "To must be after From.")); }
    else if (to.date() - from.date()).num_days() >= keep_days as i64 {
        errors.push(err("from", &format!("Logs are kept for {keep_days} day{}.", if keep_days == 1 { "" } else { "s" })));
    }
    let groups = parse_ids::<u16>(&q.groups).unwrap_or_else(|e| { errors.push(err("groups", &e)); Vec::new() });
    let radios = parse_ids::<u32>(&q.radios).unwrap_or_else(|e| { errors.push(err("radios", &e)); Vec::new() });
    if !errors.is_empty() { return Err(errors); }
    let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
    Ok(Filter {
        system: some(&q.system),
        event: some(&q.event),
        encrypted: match q.encryption.as_str() { "encrypted" => Some(true), "clear" => Some(false), _ => None },
        emergency_only: !q.emergency.is_empty(),
        groups,
        radios,
        from,
        to,
    })
}

pub async fn page(State(st): State<AppState>, Authed(s, _): Authed, Query(mut q): Query<EventLogQuery>) -> Markup {
    let settings = st.db.settings();
    let Some(dir) = settings.event_log_path() else {
        return layout("Event Log", Some(&s.user), &s.csrf, html! {
            p { "The call event log is off. Set its folder on the " a href="/recordings" { "Recordings" } " page." }
        });
    };
    let now = Local::now().naive_local();
    let result = match filter(&q, settings.event_log_days, now) {
        Ok(f) => {
            if q.from.is_empty() { q.from = f.from.format(INPUT).to_string(); }
            if q.to.is_empty() { q.to = f.to.format(INPUT).to_string(); }
            let page = page_no(&q);
            match tokio::task::spawn_blocking(move || search(&dir, &f, page, PER_PAGE)).await {
                Ok(Ok(found)) => Ok(found),
                Ok(Err(e)) => Err(vec![err("form", &format!("Could not read the event logs: {e}"))]),
                Err(e) => Err(vec![err("form", &format!("Could not read the event logs: {e}"))]),
            }
        }
        Err(errors) => Err(errors),
    };
    let systems: Vec<String> = st.db.systems().into_iter().map(|x| x.system_name).collect();
    view(&s, &q, &systems, &result)
}

fn view(s: &crate::auth::Session, q: &EventLogQuery, systems: &[String], result: &Result<Found, Vec<FieldError>>) -> Markup {
    let errors: &[FieldError] = result.as_ref().err().map(Vec::as_slice).unwrap_or(&[]);
    let events: &[String] = result.as_ref().map(|f| f.events.as_slice()).unwrap_or(&[]);
    let page = page_no(q);
    let link = |page: usize| format!("/event-log?{}",
        serde_urlencoded::to_string(EventLogQuery { page: page.to_string(), ..q.clone() }).unwrap_or_default());
    layout("Event Log", Some(&s.user), &s.csrf, html! {
        (notice(errors.iter().find(|e| e.field == "form").map(|e| e.message.as_str()), None))
        form method="get" action="/event-log" {
            label { "System" select name="system" {
                option value="" { "All" }
                @for n in systems { option value=(n) selected[q.system == *n] { (n) } }
            } }
            label { "Call type" select name="event" {
                option value="" { "All" }
                @for e in events { option value=(e) selected[q.event == *e] { (e) } }
            } }
            label { "Encryption" select name="encryption" {
                option value="" { "Any" }
                option value="encrypted" selected[q.encryption == "encrypted"] { "Encrypted" }
                option value="clear" selected[q.encryption == "clear"] { "Clear" }
            } }
            label { input type="checkbox" name="emergency" value="1" checked[!q.emergency.is_empty()]; " Emergency only" }
            label { "Talkgroups" input name="groups" value=(q.groups) placeholder="Any"; }
            (field_error(errors, "groups"))
            label { "Source radio IDs" input name="radios" value=(q.radios) placeholder="Any"; }
            (field_error(errors, "radios"))
            label { "From" input type="datetime-local" name="from" value=(q.from); }
            (field_error(errors, "from"))
            label { "To" input type="datetime-local" name="to" value=(q.to); }
            (field_error(errors, "to"))
            button { "Search" }
        }
        @if let Ok(f) = result {
            p.muted {
                (f.total) " call" @if f.total != 1 { "s" } ", " (f.encrypted) " encrypted, " (f.emergency) " emergency, "
                (f.radios) " radio" @if f.radios != 1 { "s" } ", " (format!("{:.1}", f.air_ms as f64 / 1000.0)) " s on the air"
                @if f.skipped > 0 { " (" (f.skipped) " unreadable lines skipped)" }
            }
            table {
                thead { tr { th { "Time" } th { "Duration (s)" } th { "System" } th { "Talkgroup" } th { "Radio" } th { "Frequency (MHz)" }
                    th { "Channel" } th { "Type" } th { "Flags" } } }
                tbody { @for e in &f.rows { tr {
                    td { (e.start.format("%Y-%m-%d %H:%M:%S")) }
                    td { (format!("{:.1}", e.duration_ms as f64 / 1000.0)) }
                    td { (e.system) }
                    td { @if let Some(n) = &e.name { (n) " (" (e.group) ")" } @else { (e.group) } }
                    td { @if let Some(r) = e.from { (r) } }
                    td { (format_mhz(e.hz)) }
                    td { (e.channel) }
                    td { (e.event) }
                    td {
                        @if e.encrypted { span.tag { "Encrypted" } " " }
                        @if e.emergency { span.tag.bad { "Emergency" } }
                    }
                } } }
            }
            p {
                @if page > 0 && page <= MAX_PAGE + 1 { a href=(link(page - 1)) { "Newer" } " " }
                @if page < MAX_PAGE && (page + 1) * PER_PAGE < f.total { a href=(link(page + 1)) { "Older" } }
                @if page >= MAX_PAGE && f.total > (MAX_PAGE + 1) * PER_PAGE { " Narrow the search to see older calls." }
            }
        }
    })
}
