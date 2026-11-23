//! Talkgroups: names and whether to record, kept in named alias lists (`?list=SGCo`) like sdrtrunk's; each system uses
//! one list (chosen on the Systems page). Saving an existing number replaces it.
use crate::app::{check_csrf, require, AppState, Authed};
use crate::auth::Function;
use crate::auth::Session;
use crate::db::{RadioName, Talkgroup};
use crate::pages::layout::{layout, notice, read_only};
use crate::receiver::now_ms;
use crate::radio_names;
use crate::rr_csv;
use crate::streaming::guess_list;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use maud::{html, Markup};
use serde::Deserialize;

#[derive(Deserialize, Default)]
pub struct TalkgroupForm {
    #[serde(default)] csrf: String,
    #[serde(default, rename = "list")] system: String,
    #[serde(default)] number: String,
    #[serde(default)] name: String,
    /// Checkbox: present ("on") when ticked.
    record: Option<String>,
}

/// `/talkgroups?list=NAME` plus `extra` ("&sort=name&dir=asc").
fn url(list: &str, extra: &str) -> String {
    format!("/talkgroups?{}{extra}", serde_urlencoded::to_string([("list", list)]).unwrap_or_default())
}

/// Changes must name an alias list that exists (page views fall back to the first list instead).
fn known(st: &AppState, list: &str) -> bool { st.db.alias_lists().iter().any(|n| n == list) }

/// Value of the "Import into" choice that creates a list.
const NEW_LIST: &str = "+new";

/// The RadioReference import form: into this list, another list, or a new one.
fn import_form(s: &Session, current: Option<&str>, lists: &[String]) -> Markup {
    html! {
        h2 { "Import from RadioReference" }
        form method="post" action="/talkgroups/import" enctype="multipart/form-data" {
            input type="hidden" name="csrf" value=(s.csrf);
            label { "Talkgroup CSV downloaded from RadioReference" input type="file" name="file" accept=".csv,text/csv" required; }
            label { "Import into"
                select name="into" {
                    @for l in lists { option value=(l) selected[Some(l.as_str()) == current] { "Alias list: " (l) } }
                    option value=(NEW_LIST) selected[current.is_none()] { "New alias list…" }
                }
            }
            label { "Name for a new alias list" input name="new_name" maxlength="60" placeholder="e.g. KSICS"; }
            p.muted { "Into an existing list: new talkgroups are added and marked NEW, and where a name differs from yours you choose "
                "which to keep; nothing is deleted and Record calls is never changed. Into a new list: every talkgroup is added; "
                "then pick the list for a system on the " a href="/systems" { "Systems" } " page." }
            button { "Import" }
        }
    }
}

fn view(st: &AppState, s: &Session, system: &str, f: &TalkgroupForm, error: Option<&str>) -> Markup {
    view_with(st, s, system, f, error, None, &Sort::default())
}

/// List order: column ("number", "name" or "record") and direction.
#[derive(Clone, Debug, Default)]
struct Sort { key: String, desc: bool }

impl Sort {
    fn from_query(sort: Option<&str>, dir: Option<&str>) -> Sort {
        let key = sort.filter(|k| ["number", "name", "record"].contains(k)).unwrap_or("number");
        Sort { key: key.into(), desc: dir == Some("desc") }
    }
    /// Header link: sorts by `key` ascending, or flips the direction when already sorted by it.
    fn link(&self, system: &str, key: &str) -> String {
        let desc = self.key_or_default() == key && !self.desc;
        url(system, &format!("&sort={key}&dir={}", if desc { "desc" } else { "asc" }))
    }
    fn key_or_default(&self) -> &str { if self.key.is_empty() { "number" } else { &self.key } }
    fn arrow(&self, key: &str) -> &'static str { if self.key_or_default() != key { "" } else if self.desc { " ▼" } else { " ▲" } }
}

fn hidden(s: &Session, list: &str) -> Markup {
    html! { input type="hidden" name="csrf" value=(s.csrf); input type="hidden" name="list" value=(list); }
}

/// `system` here is the alias list being shown.
fn view_with(st: &AppState, s: &Session, system: &str, f: &TalkgroupForm, error: Option<&str>, info: Option<&str>, sort: &Sort) -> Markup {
    let names = st.db.alias_lists();
    let users: Vec<String> = st.db.systems().iter().filter(|x| x.list_name() == system).map(|x| x.label()).collect();
    let mut rows = st.db.talkgroups(system);
    let editing = rows.iter().any(|t| t.number.to_string() == f.number.trim());
    rows.sort_by(|a, b| {
        let o = match sort.key_or_default() {
            "name" => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            "record" => a.record.cmp(&b.record),
            _ => std::cmp::Ordering::Equal,
        }.then(a.number.cmp(&b.number));
        if sort.desc { o.reverse() } else { o }
    });
    // Imported talkgroups not reviewed yet stay pinned to the top whatever the order (stable sort keeps it).
    let fresh = st.db.new_talkgroups(system);
    rows.sort_by_key(|t| !fresh.contains(&t.number));
    let unlisted = st.db.record_unlisted(system);
    let home = url(system, "");
    let can = st.db.may(&s.user, Function::TalkgroupsEdit);
    layout("Talkgroups", Some(&s.user), &s.csrf, html! {
        @if !can { (read_only(Function::TalkgroupsEdit)) }
        @if names.len() > 1 {
            p { "Alias list: " @for n in &names { @if n == system { strong { (n) } } @else { a href=(url(n, "")) { (n) } } " " } }
        } @else { p { "Alias list: " strong { (system) } } }
        p.muted { @if users.is_empty() { "No system uses this list yet; choose it for one on the " a href="/systems" { "Systems" } " page." }
            @else { "Used by " (users.join(", ")) "." } }
        form method="post" action="/talkgroups/unlisted" {
            (hidden(s, system))
            fieldset.lock disabled[!can] {
                label.check { input type="checkbox" name="record_unlisted" checked[unlisted]; "Record talkgroups not listed here" }
            }
            @if can { button { "Save" } }
        }
        p.muted { @if unlisted { "Talkgroups not listed here show as numbers and are recorded." }
            @else { "Only listed talkgroups with “Record calls” ticked are recorded." } }
        (notice(error, info))
        @if can {
            h2 { @if editing { "Edit talkgroup " (f.number.trim()) } @else { "Add talkgroup" } }
            form method="post" action="/talkgroups" {
                (hidden(s, system))
                label { "Number" input name="number" type="number" min="1" max="65535" value=(f.number) required; }
                label { "Name" input name="name" maxlength="64" value=(f.name) required; }
                label.check { input type="checkbox" name="record" checked[f.record.is_some() || f.number.is_empty()]; "Record calls" }
                button { "Save talkgroup" }
                @if editing { " " a href=(home) { "Cancel" } }
            }
        }
        @if st.db.may(&s.user, Function::TalkgroupsImport) { (import_form(s, Some(system), &names)) }
        h2 { "Listed talkgroups" }
        @if can && !fresh.is_empty() {
            form method="post" action="/talkgroups/reviewed" {
                (hidden(s, system))
                (fresh.len()) " new from an import. " button.plain { "Mark all reviewed" }
            }
        }
        @if rows.is_empty() { p.muted { "None yet." } } @else {
            @if can { form #batch method="post" action="/talkgroups/record" {
                (hidden(s, system))
                "Selected talkgroups: "
                button name="record" value="on" { "Record selected" } " "
                button name="record" value="off" { "Don't record selected" }
            } }
            table {
                thead { tr {
                    th { @if can { input #select-all type="checkbox" title="Select all"; } }
                    th { a href=(sort.link(system, "number")) { "Talkgroup ID" (sort.arrow("number")) } }
                    th { a href=(sort.link(system, "name")) { "Talkgroup Name" (sort.arrow("name")) } }
                    th { a href=(sort.link(system, "record")) { "Record" (sort.arrow("record")) } }
                    th {} th {}
                } }
                tbody { @for t in &rows { tr {
                    td { @if can { input type="checkbox" name="n" value=(t.number) form="batch" aria-label={ "Select talkgroup " (t.number) }; } }
                    td { (t.number) }
                    td { (t.name) @if fresh.contains(&t.number) { " " span.tag { "NEW" } } @if can && fresh.contains(&t.number) { " "
                        form.inline method="post" action={ "/talkgroups/" (t.number) "/reviewed" } {
                            (hidden(s, system))
                            button.plain { "Mark reviewed" }
                        } } }
                    td { @if t.record { "yes" } @else { span.muted { "no" } } }
                    td { @if can { a href=(url(system, &format!("&edit={}", t.number))) { "Edit" } } }
                    td { @if can { form method="post" action={ "/talkgroups/" (t.number) "/delete" } {
                        (hidden(s, system))
                        button.danger { "Delete" }
                    } } }
                } } }
            }
        }
        (radios_section(st, s, system, can))
    })
}

/// Names for radio IDs (a call's source) in this list: written into recordings' artist tag and shown with the number.
fn radios_section(st: &AppState, s: &Session, list: &str, can: bool) -> Markup {
    let radios = st.db.radio_names(list);
    html! {
        h2 #radios { "Radio names" }
        p.muted { "Recordings carry the name with the radio number (file names keep the number). Saving a number already listed renames it." }
        @if can {
            form method="post" action="/talkgroups/radios" {
                (hidden(s, list))
                label { "Radio ID" input name="radio" type="number" min="1" max=(MAX_RADIO) required; }
                label { "Name" input name="name" maxlength=(rr_csv::NAME_MAX) required; }
                button { "Save radio name" }
            }
        }
        @if st.db.may(&s.user, Function::TalkgroupsImport) {
            form method="post" action="/talkgroups/radios/import" enctype="multipart/form-data" {
                (hidden(s, list))
                label { "SDRTrunk playlist (default.xml)" input type="file" name="file" accept=".xml,text/xml" required; }
                label { "Its alias list, if the name differs from " (list) input name="source" maxlength="60"; }
                p.muted { "New radio names are added; where a name differs from yours you choose which to keep; nothing is deleted. "
                    "Radio ID ranges are skipped." }
                button { "Import radio names" }
            }
        }
        @if radios.is_empty() { p.muted { "None yet." } } @else {
            table {
                thead { tr { th { "Radio ID" } th { "Name" } th {} } }
                tbody { @for r in &radios { tr {
                    td { (r.radio) } td { (r.name) }
                    td { @if can { form method="post" action={ "/talkgroups/radios/" (r.radio) "/delete" } {
                        (hidden(s, list))
                        button.danger { "Remove" }
                    } } }
                } } }
            }
        }
    }
}

use crate::radio_names::MAX_RADIO;

#[derive(Deserialize)]
pub struct RadioForm {
    #[serde(default)] csrf: String,
    #[serde(default)] list: String,
    #[serde(default)] radio: String,
    #[serde(default)] name: String,
}

/// Adds or renames one radio name.
pub async fn save_radio(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<RadioForm>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsEdit) { return r; }
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    if !known(&st, &f.list) { return StatusCode::BAD_REQUEST.into_response(); }
    let refuse = |msg: &str| view(&st, &s, &f.list, &TalkgroupForm::default(), Some(msg)).into_response();
    let radio = match f.radio.trim().parse::<u32>() {
        Ok(n) if (1..=MAX_RADIO).contains(&n) => n,
        _ => return refuse(&format!("Radio IDs are 1 to {MAX_RADIO}.")),
    };
    let name = radio_names::clean_name(&f.name);
    let name = name.as_str();
    if name.is_empty() || name.chars().count() > rr_csv::NAME_MAX {
        return refuse(&format!("Radio names are 1 to {} characters.", rr_csv::NAME_MAX));
    }
    if let Err(e) = st.db.save_radio_name(&f.list, &RadioName { radio, name: name.into() }) {
        return refuse(&format!("Could not save: {e}"));
    }
    changed(&st, &s, format!("radio name {radio} in {}: {name}", f.list));
    Redirect::to(&format!("{}#radios", url(&f.list, ""))).into_response()
}

/// sdrtrunk playlist upload: adds radio names this list lacks, then asks about names that differ. The playlist's
/// alias list is the one typed in `source`, else the ones whose names lead to ours (as the stream import guesses).
pub async fn import_radios(State(st): State<AppState>, Authed(s, _): Authed, mut mp: Multipart) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsImport) { return r; }
    let (mut csrf, mut list, mut source, mut text) = (String::new(), String::new(), String::new(), String::new());
    while let Ok(Some(field)) = mp.next_field().await {
        match field.name() {
            Some("csrf") => csrf = field.text().await.unwrap_or_default(),
            Some("list") => list = field.text().await.unwrap_or_default(),
            Some("source") => source = field.text().await.unwrap_or_default(),
            Some("file") => text = field.bytes().await.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default(),
            _ => {}
        }
    }
    if let Err(code) = check_csrf(&s, &csrf) { return code.into_response(); }
    if !known(&st, &list) { return StatusCode::BAD_REQUEST.into_response(); }
    let refuse = |msg: &str| view(&st, &s, &list, &TalkgroupForm::default(), Some(msg)).into_response();
    let parsed = match radio_names::parse(&text) { Ok(p) => p, Err(e) => return refuse(&e) };
    let ours_lists = st.db.alias_lists();
    let source = source.trim();
    let theirs: Vec<RadioName> = parsed.by_list.iter()
        .filter(|(l, _)| if source.is_empty() { guess_list(l, &ours_lists).as_deref() == Some(list.as_str()) } else { l.as_str() == source })
        .flat_map(|(_, rs)| rs.iter().cloned()).collect();
    if theirs.is_empty() {
        let found: Vec<&str> = parsed.by_list.keys().map(String::as_str).collect();
        return refuse(&format!("No radio names for {list} in this playlist. Its alias lists with radio names: {}. \
            Type the one to use in “Its alias list”.", if found.is_empty() { "none".into() } else { found.join(", ") }));
    }
    // A radio in two of the playlist's lists that both lead here: the first one counts.
    let mut uniq: Vec<RadioName> = Vec::new();
    for r in theirs { if !uniq.iter().any(|u| u.radio == r.radio) { uniq.push(r); } }
    let c = radio_names::compare(&st.db.radio_names(&list), &uniq);
    if let Err(e) = st.db.add_radio_names(&list, &c.new) { return refuse(&format!("Could not import: {e}")); }
    changed(&st, &s, format!("imported {} into {list} from an sdrtrunk playlist", plural(c.new.len(), "radio name")));
    layout("Talkgroups", Some(&s.user), &s.csrf, html! {
        p.ok { "Imported radio names into " (list) ": " (plural(c.new.len(), "new name")) " added, " (c.unchanged) " unchanged, "
            (c.only_local) " only in yours (kept)." }
        @if parsed.ranges_skipped > 0 { p.muted { (plural(parsed.ranges_skipped, "radio range")) " skipped (not supported)." } }
        @if !parsed.skipped.is_empty() { details { summary { (plural(parsed.skipped.len(), "entry")) " skipped" }
            ul { @for m in &parsed.skipped { li { (m) } } } } }
        @if c.conflicts.is_empty() { p { a href={ (url(&list, "")) "#radios" } { "Back to " (list) } } } @else {
            h2 { "Names that differ" }
            p.muted { "Nothing changes unless you choose the playlist's name." }
            form method="post" action="/talkgroups/radios/import/apply" {
                (hidden(&s, &list))
                table {
                    thead { tr { th { "Radio ID" } th { "Yours" } th { "Playlist" } } }
                    tbody { @for (mine, pl) in &c.conflicts { tr {
                        td { (mine.radio) input type="hidden" name="conflict" value=(mine.radio);
                             input type="hidden" name={ "pl_name_" (mine.radio) } value=(pl.name); }
                        td { label.check { input type="radio" name={ "choice_" (mine.radio) } value="local" checked; "Keep: " (mine.name) } }
                        td { label.check { input type="radio" name={ "choice_" (mine.radio) } value="playlist"; "Use playlist: " (pl.name) } }
                    } } }
                }
                button { "Apply" } " "
                button.plain name="all" value="playlist" { "Use the playlist for all" }
            }
        }
    }).into_response()
}

/// Applies the choices: renames only radios set to the playlist's name; never adds or deletes.
pub async fn import_radios_apply(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<Vec<(String, String)>>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsImport) { return r; }
    let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if let Err(code) = check_csrf(&s, get("csrf").unwrap_or("")) { return code.into_response(); }
    let list = get("list").unwrap_or("");
    if !known(&st, list) { return StatusCode::BAD_REQUEST.into_response(); }
    let all = get("all") == Some("playlist");
    let existing = st.db.radio_names(list);
    let mut renames = Vec::new();
    for (_, n) in f.iter().filter(|(k, _)| k == "conflict") {
        let Ok(radio) = n.parse::<u32>() else { return StatusCode::BAD_REQUEST.into_response() };
        let name = get(&format!("pl_name_{radio}")).unwrap_or("").trim();
        if !existing.iter().any(|r| r.radio == radio) || name.is_empty() || name.chars().count() > rr_csv::NAME_MAX {
            return StatusCode::BAD_REQUEST.into_response();
        }
        if all || get(&format!("choice_{radio}")) == Some("playlist") { renames.push(RadioName { radio, name: name.into() }); }
    }
    for r in &renames {
        if let Err(e) = st.db.save_radio_name(list, r) {
            return view(&st, &s, list, &TalkgroupForm::default(), Some(&format!("Could not rename: {e}"))).into_response();
        }
    }
    if !renames.is_empty() {
        changed(&st, &s, format!("renamed {} in {list} to the sdrtrunk playlist's", plural(renames.len(), "radio")));
    }
    Redirect::to(&format!("{}#radios", url(list, ""))).into_response()
}

pub async fn delete_radio(State(st): State<AppState>, Authed(s, _): Authed, Path(radio): Path<u32>, Form(f): Form<SystemOnly>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsEdit) { return r; }
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    if !known(&st, &f.system) { return StatusCode::BAD_REQUEST.into_response(); }
    if let Err(e) = st.db.delete_radio_name(&f.system, radio) {
        return view(&st, &s, &f.system, &TalkgroupForm::default(), Some(&format!("Could not remove: {e}"))).into_response();
    }
    changed(&st, &s, format!("removed radio name {radio} from {}", f.system));
    Redirect::to(&format!("{}#radios", url(&f.system, ""))).into_response()
}

#[derive(Deserialize)]
pub struct EditQuery { #[serde(rename = "list")] system: Option<String>, edit: Option<u16>, imported: Option<usize>, created: Option<u8>, sort: Option<String>, dir: Option<String> }

/// `?edit=N` loads talkgroup N into the form; saving it replaces the entry. Without systems there is no list yet.
pub async fn page(State(st): State<AppState>, Authed(s, _): Authed, Query(q): Query<EditQuery>) -> Markup {
    let names = st.db.alias_lists();
    // Default: the first system's list, else the first list.
    let first_system = st.db.systems().first().map(|x| x.list_name().to_string()).filter(|n| names.contains(n));
    let Some(system) = q.system.filter(|n| names.contains(n)).or(first_system).or_else(|| names.first().cloned()) else {
        return layout("Talkgroups", Some(&s.user), &s.csrf, html! {
            p.muted { "No alias lists yet. Import one from RadioReference"
                @if st.db.may(&s.user, Function::SystemsAdd) { ", or " a href="/systems/new" { "add a system" } " (it gets a list named after it)" } "." }
            @if st.db.may(&s.user, Function::TalkgroupsImport) { (import_form(&s, None, &names)) }
        });
    };
    let f = q.edit.and_then(|n| st.db.talkgroups(&system).into_iter().find(|t| t.number == n)).map_or_else(TalkgroupForm::default, |t| TalkgroupForm {
        csrf: String::new(), system: system.clone(), number: t.number.to_string(), name: t.name, record: t.record.then(|| "on".into()),
    });
    let info = q.imported.map(|n| if q.created.is_some() {
        format!("New alias list {system} created with {n} talkgroups from RadioReference. Choose it for a system on the Systems page.")
    } else { format!("Imported from RadioReference: {n} new talkgroups added (marked NEW below).") });
    view_with(&st, &s, &system, &f, None, info.as_deref(), &Sort::from_query(q.sort.as_deref(), q.dir.as_deref()))
}

/// After a change: every running system reloads its list, and the change is audited.
fn changed(st: &AppState, s: &Session, action: String) {
    st.receivers.refresh_policy(&st.db);
    st.db.audit(now_ms(), &s.user.username, &action);
}

/// Batch Record calls: `n` repeated for each selected talkgroup, `record` = "on" or "off".
pub async fn record_batch(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<Vec<(String, String)>>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsEdit) { return r; }
    let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if let Err(code) = check_csrf(&s, get("csrf").unwrap_or("")) { return code.into_response(); }
    let system = get("list").unwrap_or("");
    if !known(&st, system) { return StatusCode::BAD_REQUEST.into_response(); }
    let record = match get("record") { Some("on") => true, Some("off") => false, _ => return StatusCode::BAD_REQUEST.into_response() };
    let Ok(numbers) = f.iter().filter(|(k, _)| k == "n").map(|(_, v)| v.parse::<u16>()).collect::<Result<Vec<_>, _>>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if numbers.is_empty() { return Redirect::to(&url(system, "")).into_response(); }
    if let Err(e) = st.db.set_record(system, &numbers, record) {
        return view(&st, &s, system, &TalkgroupForm::default(), Some(&format!("Could not save: {e}"))).into_response();
    }
    let what = if record { "record" } else { "not record" };
    changed(&st, &s, format!("set {} on {system} to {what}", plural(numbers.len(), "talkgroup")));
    Redirect::to(&url(system, "")).into_response()
}

fn plural(n: usize, word: &str) -> String { format!("{n} {word}{}", if n == 1 { "" } else { "s" }) }

/// RadioReference CSV upload: adds talkgroups the list does not have (marked NEW), then asks about names that differ.
pub async fn import(State(st): State<AppState>, Authed(s, _): Authed, mut mp: Multipart) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsImport) { return r; }
    let (mut csrf, mut into, mut new_name, mut text) = (String::new(), String::new(), String::new(), String::new());
    while let Ok(Some(field)) = mp.next_field().await {
        match field.name() {
            Some("csrf") => csrf = field.text().await.unwrap_or_default(),
            Some("into") => into = field.text().await.unwrap_or_default(),
            Some("new_name") => new_name = field.text().await.unwrap_or_default(),
            Some("file") => text = field.bytes().await.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default(),
            _ => {}
        }
    }
    if let Err(code) = check_csrf(&s, &csrf) { return code.into_response(); }
    let lists = st.db.alias_lists();
    // Errors before anything is stored go back to the list page (or the empty page when there is no list yet).
    let refuse = |msg: &str| match lists.first() {
        Some(first) => view(&st, &s, if lists.contains(&into) { &into } else { first }, &TalkgroupForm::default(), Some(msg)).into_response(),
        None => layout("Talkgroups", Some(&s.user), &s.csrf, html! { p.error { (msg) } (import_form(&s, None, &lists)) }).into_response(),
    };
    let creating = into == NEW_LIST;
    let system = if creating {
        let name = new_name.trim();
        if name.is_empty() || name.chars().count() > 60 { return refuse("Give the new alias list a name of 1 to 60 characters."); }
        if lists.iter().any(|l| l.eq_ignore_ascii_case(name)) { return refuse(&format!("An alias list named {name} already exists; choose it instead.")); }
        name.to_string()
    } else if known(&st, &into) { into.clone() } else { return StatusCode::BAD_REQUEST.into_response() };
    let (rows, skipped) = match rr_csv::parse(&text) {
        Ok(r) => r,
        Err(e) => return refuse(&e),
    };
    if creating {
        if let Err(e) = st.db.create_alias_list(&system) { return refuse(&format!("Could not create the list: {e}")); }
    }
    let c = rr_csv::compare(&st.db.talkgroups(&system), &rows);
    let added: Vec<Talkgroup> = c.new.iter().map(|r| Talkgroup { number: r.number, name: r.name.clone(), record: !r.encrypted }).collect();
    if let Err(e) = st.db.import_talkgroups(&system, &added) {
        return view(&st, &s, &system, &TalkgroupForm::default(), Some(&format!("Could not import: {e}"))).into_response();
    }
    changed(&st, &s, format!("imported {} into {}{system} from RadioReference, {}", plural(added.len(), "new talkgroup"),
        if creating { "new alias list " } else { "" }, plural(c.conflicts.len(), "conflict")));
    if c.conflicts.is_empty() && skipped.is_empty() {
        return Redirect::to(&url(&system, &format!("&imported={}{}", added.len(), if creating { "&created=1" } else { "" }))).into_response();
    }
    layout("RadioReference import", Some(&s.user), &s.csrf, html! {
        p.ok { (system) ": " (plural(added.len(), "new talkgroup")) " added (marked NEW in the list). "
            (c.unchanged) " unchanged, " (c.only_local) " only in this list (kept)." }
        @if !skipped.is_empty() { details { summary { (plural(skipped.len(), "row")) " skipped" } ul { @for m in &skipped { li { (m) } } } } }
        @if c.conflicts.is_empty() { p { a href=(url(&system, "")) { "Back to talkgroups" } } } @else {
            h2 { "Names that differ" }
            p.muted { "Nothing changes unless you choose RadioReference's name. Record calls stays as it is." }
            form method="post" action="/talkgroups/import/apply" {
                (hidden(&s, &system))
                table {
                    thead { tr { th { "Number" } th { "This list" } th { "RadioReference" } } }
                    tbody { @for (mine, rr) in &c.conflicts { tr {
                        td { (mine.number) input type="hidden" name="conflict" value=(mine.number);
                             input type="hidden" name={ "rr_name_" (mine.number) } value=(rr.name); }
                        td { label.check { input type="radio" name={ "choice_" (mine.number) } value="local" checked; "Keep: " (mine.name) } }
                        td { label.check { input type="radio" name={ "choice_" (mine.number) } value="rr"; "Use RR: " (rr.name) } }
                    } } }
                }
                button { "Apply choices" } " "
                button.plain name="all" value="rr" { "Use RadioReference for all" }
            }
        }
    }).into_response()
}

/// Applies the conflict choices: renames only talkgroups set to RadioReference's name; never adds or deletes.
pub async fn import_apply(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<Vec<(String, String)>>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsImport) { return r; }
    let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if let Err(code) = check_csrf(&s, get("csrf").unwrap_or("")) { return code.into_response(); }
    let system = get("list").unwrap_or("");
    if !known(&st, system) { return StatusCode::BAD_REQUEST.into_response(); }
    let all_rr = get("all") == Some("rr");
    let existing = st.db.talkgroups(system);
    let mut renames = Vec::new();
    for (_, n) in f.iter().filter(|(k, _)| k == "conflict") {
        let Ok(number) = n.parse::<u16>() else { return StatusCode::BAD_REQUEST.into_response() };
        let name = get(&format!("rr_name_{number}")).unwrap_or("").trim();
        if !existing.iter().any(|t| t.number == number) || name.is_empty() || name.chars().count() > rr_csv::NAME_MAX {
            return StatusCode::BAD_REQUEST.into_response();
        }
        if all_rr || get(&format!("choice_{number}")) == Some("rr") { renames.push((number, name.to_string())); }
    }
    for (number, name) in &renames {
        if let Err(e) = st.db.rename_talkgroup(system, *number, name) {
            return view(&st, &s, system, &TalkgroupForm::default(), Some(&format!("Could not rename: {e}"))).into_response();
        }
    }
    if !renames.is_empty() { changed(&st, &s, format!("renamed {} on {system} from RadioReference", plural(renames.len(), "talkgroup"))); }
    Redirect::to(&url(system, "")).into_response()
}

#[derive(Deserialize)]
pub struct SystemOnly { #[serde(default)] csrf: String, #[serde(default, rename = "list")] system: String }

/// Clears the NEW mark of one talkgroup (`/talkgroups/{n}/reviewed`) or of all (`/talkgroups/reviewed`).
pub async fn reviewed_one(State(st): State<AppState>, Authed(s, _): Authed, Path(number): Path<u16>, Form(f): Form<SystemOnly>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsEdit) { return r; }
    reviewed(&st, &s, Some(number), &f)
}

pub async fn reviewed_all(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<SystemOnly>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsEdit) { return r; }
    reviewed(&st, &s, None, &f)
}

fn reviewed(st: &AppState, s: &Session, number: Option<u16>, f: &SystemOnly) -> Response {
    if let Err(code) = check_csrf(s, &f.csrf) { return code.into_response(); }
    if !known(st, &f.system) { return StatusCode::BAD_REQUEST.into_response(); }
    if let Err(e) = st.db.mark_reviewed(&f.system, number) {
        return view(st, s, &f.system, &TalkgroupForm::default(), Some(&format!("Could not save: {e}"))).into_response();
    }
    Redirect::to(&url(&f.system, "")).into_response()
}

pub async fn save(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<TalkgroupForm>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsEdit) { return r; }
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    if !known(&st, &f.system) { return StatusCode::BAD_REQUEST.into_response(); }
    let number = match f.number.trim().parse::<u16>() {
        Ok(n) if n > 0 => n,
        _ => return view(&st, &s, &f.system, &f, Some("Talkgroup numbers are 1 to 65535.")).into_response(),
    };
    let name = f.name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return view(&st, &s, &f.system, &f, Some("Names are 1 to 64 characters.")).into_response();
    }
    if let Err(e) = st.db.upsert_talkgroup(&f.system, &Talkgroup { number, name: name.into(), record: f.record.is_some() }) {
        return view(&st, &s, &f.system, &f, Some(&format!("Could not save: {e}"))).into_response();
    }
    changed(&st, &s, format!("talkgroup {number} on {} saved", f.system));
    Redirect::to(&url(&f.system, "")).into_response()
}

#[derive(Deserialize)]
pub struct UnlistedForm { #[serde(default)] csrf: String, #[serde(default, rename = "list")] system: String, record_unlisted: Option<String> }

/// The per-system "record unlisted talkgroups" switch. Applies to the next call; decoders keep running.
pub async fn unlisted(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<UnlistedForm>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsEdit) { return r; }
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    if !known(&st, &f.system) { return StatusCode::BAD_REQUEST.into_response(); }
    let on = f.record_unlisted.is_some();
    if let Err(e) = st.db.set_record_unlisted(&f.system, on) {
        return view(&st, &s, &f.system, &TalkgroupForm::default(), Some(&format!("Could not save: {e}"))).into_response();
    }
    changed(&st, &s, format!("{} recording unlisted talkgroups on {}", if on { "started" } else { "stopped" }, f.system));
    Redirect::to(&url(&f.system, "")).into_response()
}

pub async fn delete(State(st): State<AppState>, Authed(s, _): Authed, Path(number): Path<u16>, Form(f): Form<SystemOnly>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsEdit) { return r; }
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    if !known(&st, &f.system) { return StatusCode::BAD_REQUEST.into_response(); }
    if let Err(e) = st.db.delete_talkgroup(&f.system, number) {
        return view(&st, &s, &f.system, &TalkgroupForm::default(), Some(&format!("Could not delete: {e}"))).into_response();
    }
    changed(&st, &s, format!("talkgroup {number} on {} deleted", f.system));
    Redirect::to(&url(&f.system, "")).into_response()
}
