//! Streams: the Icecast streams that play talkgroups live (from any alias lists), their connection state, and an
//! import of the streams in an sdrtrunk playlist. Passwords are stored but never shown again.
use crate::app::{check_csrf, require, AppState, Authed};
use crate::auth::Function;
use crate::auth::Session;
use crate::db::FieldError;
use crate::pages::layout::{field_error, layout, layout_with, notice, Head};
use crate::receiver::now_ms;
use crate::streaming::{guess_list, map_lists, parse_playlist, StreamConfig, StreamStatus, StreamTalkgroup};
use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use crate::pages::live_html;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use chrono::{Local, TimeZone};
use futures_core::Stream;
use std::convert::Infallible;
use maud::{html, Markup};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

fn pill(st: &StreamStatus) -> Markup {
    let class = match st.state.as_str() { "connected" => "running", "connecting" => "starting", "error" => "error", _ => "off" };
    html! { span class={ "pill state-" (class) } { (st.state) } }
}

fn clock(ms: i64) -> String { Local.timestamp_millis_opt(ms).single().map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default() }

/// Talkgroup numbers by alias list, in list order.
fn by_list(members: &[StreamTalkgroup]) -> BTreeMap<&str, Vec<u16>> {
    let mut m: BTreeMap<&str, Vec<u16>> = BTreeMap::new();
    for t in members { m.entry(t.list.as_str()).or_default().push(t.group); }
    m
}

/// "SGCo: 23 WPD West, 85; KSICS: 2368" (names from each alias list, when `named`).
fn talkgroups_text(st: &AppState, members: &[StreamTalkgroup], named: bool) -> String {
    by_list(members).into_iter().map(|(list, groups)| {
        let names = if named { st.db.talkgroups(list) } else { Vec::new() };
        let tgs = groups.iter().map(|g| names.iter().find(|t| t.number == *g).map_or(g.to_string(), |t| format!("{g} {}", t.name)));
        format!("{list}: {}", tgs.collect::<Vec<_>>().join(", "))
    }).collect::<Vec<_>>().join("; ")
}

/// Every stream with its live state (redrawn every second on the page, see `events`).
fn streams_table(st: &AppState) -> Markup {
    let streams = st.db.streams();
    html! {
        @if streams.is_empty() { p.muted { "No streams yet. Add one, or import the streams of an sdrtrunk playlist below." } } @else {
            table {
                thead { tr { th { "Stream" } th { "Server and mount" } th { "Talkgroups" } th { "State" } th { "Calls sent" } th { "Waiting" }
                    th { "Last call" } th {} } }
                tbody { @for c in &streams {
                    @let status = st.receivers.streams.status(c.id);
                    tr {
                        td { (c.name) @if !c.description.is_empty() { br; span.muted { (c.description) } } }
                        td { (c.host) ":" (c.port) (c.mount) }
                        td { (talkgroups_text(st, &c.members, true)) }
                        td { (pill(&status)) @if let Some(e) = &status.error { br; span.muted { (e) } } }
                        td { (status.sent) }
                        td { (status.queued) }
                        td { @if status.last_sent_ms > 0 { (clock(status.last_sent_ms)) } }
                        td.row-actions { a.button.quiet href={ "/streams/" (c.id) } { "Settings" } }
                    }
                } }
            }
        }
    }
}

/// The streams table, redrawn every second while the Streams page is open.
pub async fn events(State(st): State<AppState>, Authed(_, token): Authed) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    live_html(&st, token, |st| streams_table(st).into_string())
}

/// `?saved=1` after a save or delete; `?imported=N` after an import.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ListQuery { saved: Option<u8>, imported: Option<usize> }

pub async fn list(State(st): State<AppState>, Authed(s, _): Authed, Query(q): Query<ListQuery>) -> Markup {
    let head = Head {
        subtitle: Some(html! { "Icecast streams that play talkgroups live. Calls play in order, with silence in between." }),
        actions: Some(html! { a.button href="/streams/new" { "Add a stream" } }),
    };
    layout_with("Streams", head, Some(&s.user), &s.csrf, html! {
        @match q.imported {
            Some(0) => (notice(Some(concat!("No streams imported: none of them had talkgroups in an alias list you chose. Check that ",
                "the file is the playlist whose aliases name the streams as broadcast channel, and that its lists are not left out.")), None)),
            Some(n) => (notice(None, Some(&format!("Imported {n} stream{}. The streams restarted.", if n == 1 { "" } else { "s" })))),
            None => (notice(None, q.saved.is_some().then_some("Saved. The streams restarted."))),
        }
        div data-live="/streams/events" { (streams_table(&st)) }
        section.panel {
            h2 { "Import from sdrtrunk" }
            p.muted { "Upload an sdrtrunk playlist (SDRTrunk/playlist/default.xml). Its Icecast streams are added with their "
                "passwords and the talkgroups whose aliases name them as broadcast channel, from any alias list. Before "
                "anything is saved you see the list and choose which of your alias lists each sdrtrunk list becomes; a "
                "stream with the same server and mount is replaced." }
            form method="post" action="/streams/import" enctype="multipart/form-data" {
                input type="hidden" name="csrf" value=(s.csrf);
                label { "Playlist file" input type="file" name="file" accept=".xml" required; }
                div.form-actions { button { "Read the playlist" } }
            }
        }
    })
}

/// Every field as typed, so a rejected form comes back as entered (except the password, which is never sent back).
/// `groups`: (alias list, talkgroup numbers as typed), from the form's `tg:<list>` fields.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct StreamForm {
    csrf: String, enabled: Option<String>, name: String, description: String, host: String, port: String, mount: String,
    user: String, password: String, public: Option<String>, bitrate_kbps: String, sample_rate: String, delay_s: String,
    max_age_min: String,
    #[serde(skip)]
    groups: Vec<(String, String)>,
}

impl StreamForm {
    fn from(c: &StreamConfig) -> StreamForm {
        StreamForm {
            csrf: String::new(), enabled: c.enabled.then(|| "on".into()), name: c.name.clone(), description: c.description.clone(),
            host: c.host.clone(), port: c.port.to_string(), mount: c.mount.clone(), user: c.user.clone(), password: String::new(),
            public: c.public.then(|| "on".into()), bitrate_kbps: c.bitrate_kbps.to_string(), sample_rate: c.sample_rate.to_string(),
            delay_s: (c.delay_ms / 1000).to_string(), max_age_min: format!("{}", c.max_age_ms as f64 / 60_000.0),
            groups: by_list(&c.members).into_iter()
                .map(|(l, g)| (l.to_string(), g.iter().map(u16::to_string).collect::<Vec<_>>().join(", "))).collect(),
        }
    }

    /// The posted form: fixed fields by name, talkgroups from every `tg:<alias list>` field.
    fn parse(pairs: &[(String, String)]) -> StreamForm {
        let mut f: StreamForm = serde_urlencoded::to_string(pairs).ok().and_then(|q| serde_urlencoded::from_str(&q).ok()).unwrap_or_default();
        f.groups = pairs.iter().filter_map(|(k, v)| Some((k.strip_prefix("tg:")?.to_string(), v.clone()))).collect();
        f
    }

    fn typed(&self, list: &str) -> &str { self.groups.iter().find(|(l, _)| l == list).map_or("", |(_, v)| v.as_str()) }

    /// A blank password keeps the stored one.
    fn to_config(&self, stored: &StreamConfig) -> Result<StreamConfig, Vec<FieldError>> {
        let mut errors = Vec::new();
        let mut bad = |field, message: &str| errors.push(FieldError { field, message: message.into() });
        let mut c = stored.clone();
        c.enabled = self.enabled.is_some();
        c.public = self.public.is_some();
        (c.name, c.description, c.host, c.mount, c.user) = (self.name.trim().into(), self.description.trim().into(),
            self.host.trim().into(), self.mount.trim().into(), self.user.trim().into());
        if !self.password.is_empty() { c.password = self.password.clone(); }
        if c.password.is_empty() { bad("password", "Enter the source password."); }
        match self.port.trim().parse() { Ok(p) => c.port = p, Err(_) => bad("port", "Enter a port such as 8000.") }
        match self.bitrate_kbps.parse() { Ok(b) => c.bitrate_kbps = b, Err(_) => bad("bitrate_kbps", "Choose a bitrate.") }
        match self.sample_rate.parse() { Ok(r) => c.sample_rate = r, Err(_) => bad("sample_rate", "Choose 8 or 16 kHz.") }
        match self.delay_s.trim().parse::<u64>() { Ok(d) => c.delay_ms = d * 1000, Err(_) => bad("delay_ms", "Enter whole seconds, 0 for none.") }
        match self.max_age_min.trim().parse::<f64>() {
            Ok(m) if m.is_finite() && m > 0.0 => c.max_age_ms = (m * 60_000.0).round() as u64,
            _ => bad("max_age_ms", "Enter minutes, such as 10."),
        }
        c.members.clear();
        for (list, text) in &self.groups {
            for t in text.split([',', ' ', '\n']).map(str::trim).filter(|t| !t.is_empty()) {
                match t.parse() {
                    Ok(g) => c.members.push(StreamTalkgroup::new(list, g)),
                    Err(_) => { bad("talkgroups", &format!("{list}: enter talkgroup numbers separated by commas, such as 23, 85.")); break; }
                }
            }
        }
        c.members.sort();
        c.members.dedup();
        if let Err(more) = c.validate() { for e in more { if !errors.iter().any(|x| x.field == e.field) { errors.push(e); } } }
        if errors.is_empty() { Ok(c) } else { Err(errors) }
    }
}

fn form_view(st: &AppState, s: &Session, id: Option<i64>, f: &StreamForm, errors: &[FieldError]) -> Markup {
    let action = id.map_or("/streams/new".to_string(), |id| format!("/streams/{id}"));
    let has_password = id.and_then(|id| st.db.stream(id)).is_some_and(|c| !c.password.is_empty());
    let title = if id.is_some() { f.name.clone() } else { "Add a stream".into() };
    // Every alias list, plus any list the stream still names that no longer exists.
    let mut lists = st.db.alias_lists();
    for (l, _) in &f.groups { if !lists.contains(l) { lists.push(l.clone()); } }
    let head = Head { subtitle: Some(html! { "An Icecast mount and the talkgroups it plays." }), actions: None };
    layout_with(&title, head, Some(&s.user), &s.csrf, html! {
        (field_error(errors, "form"))
        form method="post" action=(action) {
            input type="hidden" name="csrf" value=(s.csrf);
            label.check { input type="checkbox" name="enabled" checked[f.enabled.is_some()]; "Stream these talkgroups" }
            label { "Name" input name="name" value=(f.name) required; }
            (field_error(errors, "name"))
            label { "Description (shown to listeners)" input name="description" value=(f.description); }
            fieldset {
                legend { "Icecast server" }
                label { "Server" input name="host" value=(f.host) placeholder="icecast.local"; }
                (field_error(errors, "host"))
                label { "Port" input name="port" inputmode="numeric" value=(f.port); }
                (field_error(errors, "port"))
                label { "Mount" input name="mount" value=(f.mount) placeholder="/scan21"; }
                (field_error(errors, "mount"))
                label { "Username" input name="user" value=(f.user) autocomplete="off"; }
                (field_error(errors, "user"))
                label { @if has_password { "Password (blank keeps the saved one)" } @else { "Password" }
                    input type="password" name="password" autocomplete="new-password"; }
                (field_error(errors, "password"))
                label.check { input type="checkbox" name="public" checked[f.public.is_some()]; "List on public directories" }
            }
            fieldset {
                legend { "Audio" }
                label { "Bitrate" select name="bitrate_kbps" { @for b in [8, 16, 24, 32, 48, 64] {
                    option value=(b) selected[f.bitrate_kbps == b.to_string()] { (b) " kbps" } } } }
                (field_error(errors, "bitrate_kbps"))
                label { "Sample rate" select name="sample_rate" {
                    option value="8000" selected[f.sample_rate == "8000"] { "8 kHz" }
                    option value="16000" selected[f.sample_rate == "16000"] { "16 kHz" } } }
                (field_error(errors, "sample_rate"))
                label { "Delay (seconds)" input name="delay_s" inputmode="numeric" value=(f.delay_s); }
                (field_error(errors, "delay_ms"))
                label { "Drop calls older than (minutes)" input name="max_age_min" inputmode="decimal" value=(f.max_age_min); }
                p.muted { "Calls play one after another; when they pile up, the oldest are dropped once this old." }
                (field_error(errors, "max_age_ms"))
            }
            fieldset {
                legend { "Talkgroups" }
                p.muted { "Numbers separated by commas, from any alias lists (the same number in two lists is two different "
                    "talkgroups). These are followed for the stream even when set not to record." }
                @for l in &lists {
                    label { (l) input name={ "tg:" (l) } value=(f.typed(l)) placeholder="none"; }
                }
                (field_error(errors, "talkgroups"))
            }
            div.form-actions { button { "Save and restart the streams" } a.button.quiet href="/streams" { "Cancel" } }
        }
        @if let Some(id) = id {
            form method="post" action={ "/streams/" (id) "/delete" } {
                input type="hidden" name="csrf" value=(s.csrf);
                button.quiet { "Delete this stream" }
            }
        }
    })
}

pub async fn new_page(State(st): State<AppState>, Authed(s, _): Authed) -> Markup {
    form_view(&st, &s, None, &StreamForm::from(&StreamConfig::default()), &[])
}

pub async fn edit_page(State(st): State<AppState>, Authed(s, _): Authed, Path(id): Path<i64>) -> Response {
    let Some(c) = st.db.stream(id) else { return (StatusCode::NOT_FOUND, "No such stream.").into_response() };
    form_view(&st, &s, Some(id), &StreamForm::from(&c), &[]).into_response()
}

fn store(st: &AppState, s: &Session, id: Option<i64>, f: &StreamForm) -> Response {
    let stored = id.and_then(|id| st.db.stream(id)).unwrap_or_default();
    let c = match f.to_config(&stored) { Ok(c) => c, Err(errors) => return form_view(st, s, id, f, &errors).into_response() };
    if let Err(e) = st.db.save_stream(&c) {
        return form_view(st, s, id, f, &[FieldError { field: "form", message: format!("Could not save: {e}") }]).into_response();
    }
    st.receivers.apply_streams(&st.db);
    st.db.audit(now_ms(), &s.user.username, &format!("saved stream {}", c.name));
    Redirect::to("/streams?saved=1").into_response()
}

pub async fn create(State(st): State<AppState>, Authed(s, _): Authed, Form(pairs): Form<Vec<(String, String)>>) -> Response {
    if let Some(r) = require(&st, &s, Function::StreamsAdd) { return r; }
    let f = StreamForm::parse(&pairs);
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    store(&st, &s, None, &f)
}

pub async fn save(State(st): State<AppState>, Authed(s, _): Authed, Path(id): Path<i64>, Form(pairs): Form<Vec<(String, String)>>) -> Response {
    if let Some(r) = require(&st, &s, Function::StreamsEdit) { return r; }
    let f = StreamForm::parse(&pairs);
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    if st.db.stream(id).is_none() { return (StatusCode::NOT_FOUND, "No such stream.").into_response(); }
    store(&st, &s, Some(id), &f)
}

#[derive(Deserialize)]
pub struct CsrfOnly { #[serde(default)] csrf: String }

pub async fn delete(State(st): State<AppState>, Authed(s, _): Authed, Path(id): Path<i64>, Form(f): Form<CsrfOnly>) -> Response {
    if let Some(r) = require(&st, &s, Function::StreamsDelete) { return r; }
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    let Some(c) = st.db.stream(id) else { return (StatusCode::NOT_FOUND, "No such stream.").into_response() };
    if let Err(e) = st.db.delete_stream(id) {
        return form_view(&st, &s, Some(id), &StreamForm::from(&c), &[FieldError { field: "form", message: format!("Could not delete: {e}") }]).into_response();
    }
    st.receivers.apply_streams(&st.db);
    st.db.audit(now_ms(), &s.user.username, &format!("deleted stream {}", c.name));
    Redirect::to("/streams?saved=1").into_response()
}

// ---------- import ----------

/// The stored stream an imported one replaces: same server, port and mount.
fn existing(st: &AppState, c: &StreamConfig) -> Option<StreamConfig> {
    st.db.streams().into_iter().find(|s| s.host == c.host && s.port == c.port && s.mount == c.mount)
}

/// Reads an uploaded playlist and shows what would be imported, with a choice of our alias list for each sdrtrunk
/// alias list; nothing is saved until confirmed.
pub async fn import(State(st): State<AppState>, Authed(s, _): Authed, mut mp: Multipart) -> Response {
    if let Some(r) = require(&st, &s, Function::StreamsImport) { return r; }
    let (mut csrf, mut text) = (String::new(), String::new());
    while let Ok(Some(field)) = mp.next_field().await {
        match field.name() {
            Some("csrf") => csrf = field.text().await.unwrap_or_default(),
            Some("file") => text = field.bytes().await.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default(),
            _ => {}
        }
    }
    if let Err(code) = check_csrf(&s, &csrf) { return code.into_response(); }
    let fail = |msg: String| layout("Import streams", Some(&s.user), &s.csrf, html! {
        (notice(Some(msg.as_str()), None)) a.button href="/streams" { "Back to Streams" }
    }).into_response();
    let (found, skipped) = match parse_playlist(&text) { Ok(r) => r, Err(e) => return fail(format!("Could not read the file: {e}")) };
    if found.is_empty() { return fail(format!("The playlist has no Icecast streams ({skipped} of other kinds).")); }
    let ours = st.db.alias_lists();
    let mut sources: Vec<&str> = found.iter().flat_map(|c| c.members.iter().map(|m| m.list.as_str())).collect();
    sources.sort_unstable();
    sources.dedup();
    let page = layout("Import streams", Some(&s.user), &s.csrf, html! {
        p { (found.len()) " Icecast streams found"
            @if skipped > 0 { ", " (skipped) " streams of other kinds (Broadcastify, Rdio, OpenMHz…) skipped" } "." }
        form method="post" action="/streams/import/apply" {
            input type="hidden" name="csrf" value=(s.csrf);
            @if !sources.is_empty() {
                fieldset {
                    legend { "Alias lists" }
                    p.muted { "The talkgroup numbers of each sdrtrunk alias list belong to one of your alias lists." }
                    @for src in &sources {
                        @let guess = guess_list(src, &ours);
                        label { "sdrtrunk list “" (src) "” is"
                            select name={ "map:" (src) } {
                                @for l in &ours { option value=(l) selected[guess.as_deref() == Some(l.as_str())] { (l) } }
                                option value="" selected[guess.is_none()] { "(leave its talkgroups out)" }
                            }
                        }
                    }
                }
            }
            table {
                thead { tr { th { "Stream" } th { "Server and mount" } th { "Talkgroups (sdrtrunk lists)" } th { "On" } th { "Result" } } }
                tbody { @for c in &found { tr {
                    td { (c.name) }
                    td { (c.host) ":" (c.port) (c.mount) }
                    td { @if c.members.is_empty() { span.muted { "none: no alias names this stream" } } @else { (talkgroups_text(&st, &c.members, false)) } }
                    td { @if c.enabled { "yes" } @else { "no" } }
                    td { @if c.members.is_empty() { "skipped" } @else if existing(&st, c).is_some() { "replaces the saved stream" } @else { "new" } }
                } } }
            }
            div.form-actions { button { "Import these streams" } a.button.quiet href="/streams" { "Cancel" } }
        }
    });
    st.receivers.streams.pending.lock().unwrap_or_else(|p| p.into_inner()).insert(s.user.username.clone(), found);
    page.into_response()
}

/// Saves the streams the user last previewed, their talkgroups moved to the chosen alias lists (streams left without
/// talkgroups are skipped).
pub async fn import_apply(State(st): State<AppState>, Authed(s, _): Authed, Form(pairs): Form<Vec<(String, String)>>) -> Response {
    if let Some(r) = require(&st, &s, Function::StreamsImport) { return r; }
    let csrf = pairs.iter().find(|(k, _)| k == "csrf").map(|(_, v)| v.as_str()).unwrap_or_default();
    if let Err(code) = check_csrf(&s, csrf) { return code.into_response(); }
    let Some(found) = st.receivers.streams.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&s.user.username) else {
        return Redirect::to("/streams").into_response();
    };
    let ours = st.db.alias_lists();
    let map: HashMap<String, String> = pairs.iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("map:")?.to_string(), v.clone())))
        .filter(|(_, v)| v.is_empty() || ours.contains(v)).collect();
    let mut saved = 0;
    for c in map_lists(found, &map).into_iter().filter(|c| !c.members.is_empty()) {
        let id = existing(&st, &c).map_or(0, |e| e.id);
        if st.db.save_stream(&StreamConfig { id, ..c }).is_ok() { saved += 1; }
    }
    st.receivers.apply_streams(&st.db);
    st.db.audit(now_ms(), &s.user.username, &format!("imported {saved} streams"));
    Redirect::to(&format!("/streams?imported={saved}")).into_response()
}
