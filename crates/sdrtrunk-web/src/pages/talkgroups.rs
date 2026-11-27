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
use crate::list_csv;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, StatusCode};
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
    format!("/talkgroups?{}{extra}", list_query(list))
}

/// `list=NAME`, URL-encoded.
fn list_query(list: &str) -> String { serde_urlencoded::to_string([("list", list)]).unwrap_or_default() }

/// Changes must name an alias list that exists (page views fall back to the first list instead).
fn known(st: &AppState, list: &str) -> bool { st.db.alias_lists().iter().any(|n| n == list) }

/// The import dropdowns start here; importing without choosing fails.
const CHOOSE_LIST: &str = "Choose Alias List...";
const NO_LIST_CHOSEN: &str = "Choose an alias list to import into.";

/// Value of the "Import into" choice that creates a list.
pub const NEW_LIST: &str = "+new";

/// The RadioReference import form: into this list, another list, or a new one.
fn import_form(s: &Session, lists: &[String]) -> Markup {
    html! {
        h2 { "Import talkgroups" }
        form method="post" action="/talkgroups/import" enctype="multipart/form-data" {
            input type="hidden" name="csrf" value=(s.csrf);
            label { "Talkgroup CSV: downloaded from RadioReference, or exported here (Export CSV below)"
                input type="file" name="file" accept=".csv,text/csv" required; }
            label { "Import into"
                select name="into" required {
                    option value="" selected { (CHOOSE_LIST) }
                    @for l in lists { option value=(l) { "Alias list: " (l) } }
                    option value=(NEW_LIST) { "New alias list…" }
                }
            }
            label { "Name for a new alias list" input name="new_name" maxlength="60" placeholder="e.g. KSICS"; }
            p.muted { "Into an existing list: new talkgroups are added and marked NEW, and where a name differs from yours you choose "
                "which to keep; nothing is deleted. A RadioReference file never changes Record calls; an exported file brings its "
                "Record calls setting, and where it differs you choose. Into a new list: every talkgroup is added; "
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
        @if st.db.may(&s.user, Function::TalkgroupsImport) { (import_form(s, &names)) }
        h2 { "Listed talkgroups" }
        p { a.button.quiet href={ "/talkgroups/export.csv?" (list_query(system)) } { "Export CSV" } }
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
                button name="record" value="off" { "Don't record selected" } " "
                button.danger formaction="/talkgroups/delete" { "Delete selected" }
            } }
            table {
                thead { tr {
                    th { @if can { input type="checkbox" data-select-all="batch" title="Select all"; } }
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
        p { a.button.quiet href={ "/talkgroups/radios/export.csv?" (list_query(list)) } { "Export CSV" } }
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
            h3 { "Import radio names from SDRTrunk" }
            form method="post" action="/talkgroups/radios/import" enctype="multipart/form-data" {
                input type="hidden" name="csrf" value=(s.csrf);
                label { "SDRTrunk playlist (default.xml)" input type="file" name="file" accept=".xml,text/xml" required; }
                p.muted { "Next you choose, for each of the playlist's alias lists, which of your lists it goes into (or not to "
                    "import it). New radio names are added; where a name differs from yours you choose which to keep; nothing "
                    "is deleted. Radio ID ranges are skipped." }
                button { "Upload playlist" }
            }
            h3 { "Import radio names from a CSV" }
            form method="post" action="/talkgroups/radios/import-csv" enctype="multipart/form-data" {
                (hidden(s, list))
                label { "Radio name CSV (as Export CSV above writes it)" input type="file" name="file" accept=".csv,text/csv" required; }
                p.muted { "Into this list (" (list) "). New radio names are added; where a name differs from yours you choose which "
                    "to keep; nothing is deleted." }
                button { "Upload CSV" }
            }
        }
        @if radios.is_empty() { p.muted { "None yet." } } @else {
            @if can { form #radio-batch method="post" action="/talkgroups/radios/delete" {
                (hidden(s, list))
                "Selected radio names: " button.danger { "Delete selected" }
            } }
            table {
                thead { tr {
                    th { @if can { input type="checkbox" data-select-all="radio-batch" title="Select all"; } }
                    th { "Radio ID" } th { "Name" } th {}
                } }
                tbody { @for r in &radios { tr {
                    td { @if can { input type="checkbox" name="r" value=(r.radio) form="radio-batch" aria-label={ "Select radio " (r.radio) }; } }
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

/// "Don't import" in the mapping step.
const SKIP: &str = "-";

/// sdrtrunk playlist import, nothing guessed. Step one sends the file; the answer lists every alias list in the playlist
/// with a dropdown of our lists (and "Don't import"), each unchosen, the playlist carried back as `xml`. Step two
/// (`src_i` = the playlist's list, `map_i` = ours or `SKIP`, all chosen) adds the names our lists lack and asks about
/// names that differ.
pub async fn import_radios(State(st): State<AppState>, Authed(s, _): Authed, mut mp: Multipart) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsImport) { return r; }
    let (mut csrf, mut text, mut fields) = (String::new(), String::new(), Vec::<(String, String)>::new());
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "csrf" => csrf = field.text().await.unwrap_or_default(),
            "file" | "xml" => text = field.bytes().await.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default(),
            _ => fields.push((name, field.text().await.unwrap_or_default())),
        }
    }
    if let Err(code) = check_csrf(&s, &csrf) { return code.into_response(); }
    let page = |body: Markup| layout("Talkgroups", Some(&s.user), &s.csrf, body).into_response();
    let parsed = match radio_names::parse(&text) {
        Ok(p) => p,
        Err(e) => return page(html! { p.error { (e) } p { a href="/talkgroups#radios" { "Back to Talkgroups" } } }),
    };
    if parsed.by_list.is_empty() {
        return page(html! { p.error { "This playlist has no radio names." } p { a href="/talkgroups#radios" { "Back to Talkgroups" } } });
    }
    let ours = st.db.alias_lists();
    let get = |k: String| fields.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
    // The mapping as sent back: (playlist list, our list or SKIP or "" when unchosen).
    let maps: Vec<(&str, &str)> = (0..).map_while(|i| Some((get(format!("src_{i}"))?, get(format!("map_{i}")).unwrap_or("")))).collect();
    if maps.is_empty() { return page(map_lists(&s, &text, &parsed, &ours, &[], None)); }
    if maps.iter().any(|(_, m)| !m.is_empty() && *m != SKIP && !ours.iter().any(|o| o == m)) { return StatusCode::BAD_REQUEST.into_response(); }
    if maps.len() != parsed.by_list.len() || maps.iter().any(|(_, m)| m.is_empty()) {
        return page(map_lists(&s, &text, &parsed, &ours, &maps, Some("Choose an alias list or Don't import for every playlist list.")));
    }
    // Per list of ours, its playlist names in file order; a radio named differently in two of them keeps the first.
    let mut into: Vec<(String, Vec<(RadioName, String)>)> = Vec::new();
    let mut notes = Vec::new();
    for (src, names) in &parsed.by_list {
        let Some(&(_, target)) = maps.iter().find(|(p, _)| p == src) else {
            return page(map_lists(&s, &text, &parsed, &ours, &maps, Some("Choose an alias list or Don't import for every playlist list.")));
        };
        if target == SKIP { continue; }
        let at = match into.iter().position(|(l, _)| l == target) {
            Some(i) => i,
            None => { into.push((target.to_string(), Vec::new())); into.len() - 1 }
        };
        for r in names {
            match into[at].1.iter().find(|(k, _)| k.radio == r.radio) {
                None => into[at].1.push((r.clone(), src.clone())),
                Some((k, from)) if k.name != r.name => notes.push(format!("radio {} in {target}: {} in {from}, {} in {src}; kept {}",
                    r.radio, k.name, r.name, k.name)),
                Some(_) => {}
            }
        }
    }
    // Notes and range counts only for the playlist lists imported.
    let imported = |l: &String| maps.iter().any(|(p, m)| p == l && *m != SKIP);
    let ranges: usize = parsed.ranges.iter().filter(|(l, _)| imported(l)).map(|(_, n)| n).sum();
    notes.extend(parsed.skipped.iter().filter(|(l, _)| imported(l)).map(|(_, m)| m.clone()));
    let mut results = Vec::new();
    let mut conflicts: Vec<(String, RadioName, RadioName)> = Vec::new();
    for (list, named) in &into {
        let theirs: Vec<RadioName> = named.iter().map(|(r, _)| r.clone()).collect();
        let c = radio_names::compare(&st.db.radio_names(list), &theirs);
        if let Err(e) = st.db.add_radio_names(list, &c.new) {
            return page(html! { p.error { "Could not import into " (list) ": " (e) } });
        }
        changed(&st, &s, format!("imported {} into {list} from an sdrtrunk playlist", plural(c.new.len(), "radio name")));
        conflicts.extend(c.conflicts.iter().map(|(o, t)| (list.clone(), o.clone(), t.clone())));
        results.push((list.clone(), c));
    }
    page(html! {
        @for (list, c) in &results {
            p.ok { "Radio names into " (list) ": " (plural(c.new.len(), "new name")) " added, " (c.unchanged) " unchanged, "
                (c.only_local) " only in yours (kept)." }
        }
        @if ranges > 0 { p.muted { (plural(ranges, "radio range")) " skipped (not supported)." } }
        @if !notes.is_empty() {
            details open { summary { (plural(notes.len(), "note")) } ul { @for m in &notes { li { (m) } } } }
        }
        @if conflicts.is_empty() { p { a href="/talkgroups#radios" { "Back to Talkgroups" } } } @else {
            h2 { "Names that differ" }
            p.muted { "Nothing changes unless you choose the playlist's name." }
            form method="post" action="/talkgroups/radios/import/apply" {
                input type="hidden" name="csrf" value=(s.csrf);
                table {
                    thead { tr { th { "List" } th { "Radio ID" } th { "Yours" } th { "Playlist" } } }
                    tbody { @for (i, (list, mine, pl)) in conflicts.iter().enumerate() { tr {
                        td { (list) input type="hidden" name="conflict" value=(i);
                             input type="hidden" name={ "list_" (i) } value=(list);
                             input type="hidden" name={ "radio_" (i) } value=(mine.radio);
                             input type="hidden" name={ "pl_name_" (i) } value=(pl.name); }
                        td { (mine.radio) }
                        td { label.check { input type="radio" name={ "choice_" (i) } value="local" checked; "Keep: " (mine.name) } }
                        td { label.check { input type="radio" name={ "choice_" (i) } value="playlist"; "Use playlist: " (pl.name) } }
                    } } }
                }
                button { "Apply" } " "
                button.plain name="all" value="playlist" { "Use the playlist for all" }
            }
        }
    })
}

/// The mapping step: every alias list in the playlist (file order) with its radio count and a dropdown of our lists,
/// "Don't import", starting on "Choose Alias List..." unless `chosen` (sent back after an error) says otherwise.
fn map_lists(s: &Session, xml: &str, parsed: &radio_names::Parsed, ours: &[String], chosen: &[(&str, &str)], error: Option<&str>) -> Markup {
    html! {
        h2 { "Import radio names from SDRTrunk" }
        (notice(error, None))
        form method="post" action="/talkgroups/radios/import" enctype="multipart/form-data" {
            input type="hidden" name="csrf" value=(s.csrf);
            input type="hidden" name="xml" value=(xml);
            table {
                thead { tr { th { "Playlist alias list" } th { "Radio names" } th { "Import into" } } }
                tbody { @for (i, (src, names)) in parsed.by_list.iter().enumerate() {
                    @let pick = chosen.iter().find(|(p, _)| p == src).map_or("", |(_, m)| *m);
                    tr {
                        td { (src) input type="hidden" name={ "src_" (i) } value=(src); }
                        td { (names.len()) }
                        td { select name={ "map_" (i) } required {
                            option value="" selected[pick.is_empty()] { (CHOOSE_LIST) }
                            @for o in ours { option value=(o) selected[pick == o] { "Alias list: " (o) } }
                            option value=(SKIP) selected[pick == SKIP] { "Don't import" }
                        } }
                    }
                } }
            }
            button { "Import" } " " a href="/talkgroups#radios" { "Cancel" }
        }
    }
}

/// Applies the choices: renames only radios set to the playlist's name; never adds or deletes.
pub async fn import_radios_apply(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<Vec<(String, String)>>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsImport) { return r; }
    let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if let Err(code) = check_csrf(&s, get("csrf").unwrap_or("")) { return code.into_response(); }
    let all = get("all") == Some("playlist");
    let mut renames = Vec::new();
    for (_, i) in f.iter().filter(|(k, _)| k == "conflict") {
        let list = get(&format!("list_{i}")).unwrap_or("");
        if !known(&st, list) { return StatusCode::BAD_REQUEST.into_response(); }
        let Some(Ok(radio)) = get(&format!("radio_{i}")).map(str::parse::<u32>) else { return StatusCode::BAD_REQUEST.into_response() };
        let name = radio_names::clean_name(get(&format!("pl_name_{i}")).unwrap_or(""));
        if !st.db.radio_names(list).iter().any(|r| r.radio == radio) || name.is_empty() || name.chars().count() > rr_csv::NAME_MAX {
            return StatusCode::BAD_REQUEST.into_response();
        }
        if all || get(&format!("choice_{i}")) == Some("playlist") { renames.push((list.to_string(), RadioName { radio, name })); }
    }
    for (list, r) in &renames {
        if let Err(e) = st.db.save_radio_name(list, r) {
            return view(&st, &s, list, &TalkgroupForm::default(), Some(&format!("Could not rename: {e}"))).into_response();
        }
    }
    if !renames.is_empty() {
        changed(&st, &s, format!("renamed {} to the sdrtrunk playlist's names", plural(renames.len(), "radio")));
    }
    Redirect::to("/talkgroups#radios").into_response()
}

/// Radio name CSV upload into the list on screen: adds the names the list lacks, then asks about names that differ.
pub async fn import_radios_csv(State(st): State<AppState>, Authed(s, _): Authed, mut mp: Multipart) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsImport) { return r; }
    let (mut csrf, mut list, mut text) = (String::new(), String::new(), String::new());
    while let Ok(Some(field)) = mp.next_field().await {
        match field.name() {
            Some("csrf") => csrf = field.text().await.unwrap_or_default(),
            Some("list") => list = field.text().await.unwrap_or_default(),
            Some("file") => text = field.bytes().await.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default(),
            _ => {}
        }
    }
    if let Err(code) = check_csrf(&s, &csrf) { return code.into_response(); }
    if !known(&st, &list) { return StatusCode::BAD_REQUEST.into_response(); }
    let back = |msg: &str| view(&st, &s, &list, &TalkgroupForm::default(), Some(msg)).into_response();
    let (rows, skipped) = match list_csv::parse_radios(&text) {
        Ok(r) => r,
        Err(e) => return back(&e),
    };
    if rows.is_empty() { return back("The file has no radio names."); }
    let c = radio_names::compare(&st.db.radio_names(&list), &rows);
    if let Err(e) = st.db.add_radio_names(&list, &c.new) { return back(&format!("Could not import: {e}")); }
    changed(&st, &s, format!("imported {} into {list} from a CSV", plural(c.new.len(), "radio name")));
    let home = format!("{}#radios", url(&list, ""));
    if c.conflicts.is_empty() && skipped.is_empty() { return Redirect::to(&home).into_response(); }
    layout("Radio name CSV import", Some(&s.user), &s.csrf, html! {
        p.ok { (list) ": " (plural(c.new.len(), "new name")) " added, " (c.unchanged) " unchanged, " (c.only_local) " only in this list (kept)." }
        @if !skipped.is_empty() { details { summary { (plural(skipped.len(), "row")) " skipped" } ul { @for m in &skipped { li { (m) } } } } }
        @if c.conflicts.is_empty() { p { a href=(home) { "Back to radio names" } } } @else {
            h2 { "Names that differ" }
            p.muted { "Nothing changes unless you choose the file's name." }
            form method="post" action="/talkgroups/radios/import-csv/apply" {
                (hidden(&s, &list))
                table {
                    thead { tr { th { "Radio ID" } th { "This list" } th { "File" } } }
                    tbody { @for (mine, file) in &c.conflicts { tr {
                        td { (mine.radio) input type="hidden" name="conflict" value=(mine.radio);
                             input type="hidden" name={ "file_name_" (mine.radio) } value=(file.name); }
                        td { label.check { input type="radio" name={ "choice_" (mine.radio) } value="local" checked; "Keep: " (mine.name) } }
                        td { label.check { input type="radio" name={ "choice_" (mine.radio) } value="file"; "Use file: " (file.name) } }
                    } } }
                }
                button { "Apply choices" } " "
                button.plain name="all" value="file" { "Use the file for all" }
            }
        }
    }).into_response()
}

/// Applies the radio name CSV review: radios set to the file (or all, with `all=file`) take its name. Every listed conflict
/// must be a radio of the list with a valid name; never adds or deletes.
pub async fn import_radios_csv_apply(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<Vec<(String, String)>>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsImport) { return r; }
    let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if let Err(code) = check_csrf(&s, get("csrf").unwrap_or("")) { return code.into_response(); }
    let list = get("list").unwrap_or("");
    if !known(&st, list) { return StatusCode::BAD_REQUEST.into_response(); }
    let all = get("all") == Some("file");
    let existing = st.db.radio_names(list);
    let mut renames = Vec::new();
    for (_, r) in f.iter().filter(|(k, _)| k == "conflict") {
        let Ok(radio) = r.parse::<u32>() else { return StatusCode::BAD_REQUEST.into_response() };
        let name = list_csv::check_name(get(&format!("file_name_{radio}")).unwrap_or(""));
        let (Ok(name), true) = (name, existing.iter().any(|e| e.radio == radio)) else { return StatusCode::BAD_REQUEST.into_response() };
        if all || get(&format!("choice_{radio}")) == Some("file") { renames.push(RadioName { radio, name }); }
    }
    for r in &renames {
        if let Err(e) = st.db.save_radio_name(list, r) {
            return view(&st, &s, list, &TalkgroupForm::default(), Some(&format!("Could not rename: {e}"))).into_response();
        }
    }
    if !renames.is_empty() { changed(&st, &s, format!("renamed {} on {list} from a CSV", plural(renames.len(), "radio"))); }
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
pub struct EditQuery { #[serde(rename = "list")] system: Option<String>, edit: Option<u16>, imported: Option<usize>, created: Option<u8>,
    /// `csv`: the import was this program's own talkgroup CSV, not RadioReference's.
    from: Option<String>, sort: Option<String>, dir: Option<String> }

/// `?edit=N` loads talkgroup N into the form; saving it replaces the entry. Without systems there is no list yet.
pub async fn page(State(st): State<AppState>, Authed(s, _): Authed, Query(q): Query<EditQuery>) -> Markup {
    let names = st.db.alias_lists();
    // Default: the first system's list, else the first list.
    let first_system = st.db.systems().first().map(|x| x.list_name().to_string()).filter(|n| names.contains(n));
    let Some(system) = q.system.filter(|n| names.contains(n)).or(first_system).or_else(|| names.first().cloned()) else {
        return layout("Talkgroups", Some(&s.user), &s.csrf, html! {
            p.muted { "No alias lists yet. Import one from RadioReference"
                @if st.db.may(&s.user, Function::SystemsAdd) { ", or " a href="/systems/new" { "add a system" } " (it gets a list named after it)" } "." }
            @if st.db.may(&s.user, Function::TalkgroupsImport) { (import_form(&s, &names)) }
        });
    };
    let f = q.edit.and_then(|n| st.db.talkgroups(&system).into_iter().find(|t| t.number == n)).map_or_else(TalkgroupForm::default, |t| TalkgroupForm {
        csrf: String::new(), system: system.clone(), number: t.number.to_string(), name: t.name, record: t.record.then(|| "on".into()),
    });
    let source = if q.from.as_deref() == Some("csv") { "a talkgroup CSV" } else { "RadioReference" };
    let info = q.imported.map(|n| if q.created.is_some() {
        format!("New alias list {system} created with {n} talkgroups from {source}. Choose it for a system on the Systems page.")
    } else { format!("Imported from {source}: {n} new talkgroups added (marked NEW below).") });
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
        None => layout("Talkgroups", Some(&s.user), &s.csrf, html! { p.error { (msg) } (import_form(&s, &lists)) }).into_response(),
    };
    if into.is_empty() { return refuse(NO_LIST_CHOSEN); }
    let creating = into == NEW_LIST;
    let system = if creating {
        let name = new_name.trim();
        if name.is_empty() || name.chars().count() > 60 { return refuse("Give the new alias list a name of 1 to 60 characters."); }
        if lists.iter().any(|l| l.eq_ignore_ascii_case(name)) { return refuse(&format!("An alias list named {name} already exists; choose it instead.")); }
        name.to_string()
    } else if known(&st, &into) { into.clone() } else { return StatusCode::BAD_REQUEST.into_response() };
    if list_csv::is_talkgroup_file(&text) { return import_file(&st, &s, &text, &system, creating, &refuse); }
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

/// `import` for this program's own talkgroup CSV: adds the talkgroups the list lacks (the file's Record calls, marked
/// NEW), then asks about those whose name or Record calls differs. Nothing else changes before that choice.
fn import_file(st: &AppState, s: &Session, text: &str, system: &str, creating: bool, refuse: &dyn Fn(&str) -> Response) -> Response {
    let (rows, skipped) = match list_csv::parse_talkgroups(text) {
        Ok(r) => r,
        Err(e) => return refuse(&e),
    };
    if rows.is_empty() { return refuse("The file has no talkgroups."); }
    if creating {
        if let Err(e) = st.db.create_alias_list(system) { return refuse(&format!("Could not create the list: {e}")); }
    }
    let c = list_csv::compare_talkgroups(&st.db.talkgroups(system), &rows);
    if let Err(e) = st.db.import_talkgroups(system, &c.new) {
        return view(st, s, system, &TalkgroupForm::default(), Some(&format!("Could not import: {e}"))).into_response();
    }
    changed(st, s, format!("imported {} into {}{system} from a talkgroup CSV, {}", plural(c.new.len(), "new talkgroup"),
        if creating { "new alias list " } else { "" }, plural(c.conflicts.len(), "conflict")));
    if c.conflicts.is_empty() && skipped.is_empty() {
        return Redirect::to(&url(system, &format!("&imported={}&from=csv{}", c.new.len(), if creating { "&created=1" } else { "" }))).into_response();
    }
    let rec = |t: &Talkgroup| if t.record { "record" } else { "don't record" };
    layout("Talkgroup CSV import", Some(&s.user), &s.csrf, html! {
        p.ok { (system) ": " (plural(c.new.len(), "new talkgroup")) " added (marked NEW in the list). "
            (c.unchanged) " unchanged, " (c.only_local) " only in this list (kept)." }
        @if !skipped.is_empty() { details { summary { (plural(skipped.len(), "row")) " skipped" } ul { @for m in &skipped { li { (m) } } } } }
        @if c.conflicts.is_empty() { p { a href=(url(system, "")) { "Back to talkgroups" } } } @else {
            h2 { "Talkgroups that differ" }
            p.muted { "Nothing changes unless you choose the file. Use file takes both its name and its Record calls setting." }
            form method="post" action="/talkgroups/import/file/apply" {
                (hidden(s, system))
                table {
                    thead { tr { th { "Number" } th { "This list" } th { "File" } } }
                    tbody { @for (mine, file) in &c.conflicts { tr {
                        td { (mine.number) input type="hidden" name="conflict" value=(mine.number);
                             input type="hidden" name={ "file_name_" (mine.number) } value=(file.name);
                             input type="hidden" name={ "file_record_" (mine.number) } value=(if file.record { "yes" } else { "no" }); }
                        td { label.check { input type="radio" name={ "choice_" (mine.number) } value="local" checked;
                            "Keep: " (mine.name) " (" (rec(mine)) ")" } }
                        td { label.check { input type="radio" name={ "choice_" (mine.number) } value="file";
                            "Use file: " (file.name) " (" (rec(file)) ")" } }
                    } } }
                }
                button { "Apply choices" } " "
                button.plain name="all" value="file" { "Use the file for all" }
            }
        }
    }).into_response()
}

/// Applies the own-CSV review: talkgroups set to the file (or all, with `all=file`) take its name and Record calls.
/// Every listed conflict must be a talkgroup of the list with valid file values; never adds or deletes.
pub async fn import_file_apply(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<Vec<(String, String)>>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsImport) { return r; }
    let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if let Err(code) = check_csrf(&s, get("csrf").unwrap_or("")) { return code.into_response(); }
    let system = get("list").unwrap_or("");
    if !known(&st, system) { return StatusCode::BAD_REQUEST.into_response(); }
    let all = get("all") == Some("file");
    let existing = st.db.talkgroups(system);
    let mut updates = Vec::new();
    for (_, n) in f.iter().filter(|(k, _)| k == "conflict") {
        let Ok(number) = n.parse::<u16>() else { return StatusCode::BAD_REQUEST.into_response() };
        let name = list_csv::check_name(get(&format!("file_name_{number}")).unwrap_or(""));
        let record = list_csv::parse_record(get(&format!("file_record_{number}")).unwrap_or(""));
        let (Ok(name), Some(record), true) = (name, record, existing.iter().any(|t| t.number == number)) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if all || get(&format!("choice_{number}")) == Some("file") { updates.push(Talkgroup { number, name, record }); }
    }
    for t in &updates {
        let done = st.db.rename_talkgroup(system, t.number, &t.name).and_then(|_| st.db.set_record(system, &[t.number], t.record));
        if let Err(e) = done {
            return view(&st, &s, system, &TalkgroupForm::default(), Some(&format!("Could not update: {e}"))).into_response();
        }
    }
    if !updates.is_empty() { changed(&st, &s, format!("updated {} on {system} from a talkgroup CSV", plural(updates.len(), "talkgroup"))); }
    Redirect::to(&url(system, "")).into_response()
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

#[derive(Deserialize)]
pub struct ListQuery { #[serde(default)] list: String }

/// `GET /talkgroups/export.csv?list=L`: the list's talkgroups as this program's CSV, for anyone signed in.
pub async fn export_talkgroups(State(st): State<AppState>, Authed(_, _): Authed, Query(q): Query<ListQuery>) -> Response {
    if !known(&st, &q.list) { return StatusCode::NOT_FOUND.into_response(); }
    csv_download(&q.list, "talkgroups", list_csv::write_talkgroups(&st.db.talkgroups(&q.list)))
}

/// `GET /talkgroups/radios/export.csv?list=L`: the list's radio names as this program's CSV, for anyone signed in.
pub async fn export_radios(State(st): State<AppState>, Authed(_, _): Authed, Query(q): Query<ListQuery>) -> Response {
    if !known(&st, &q.list) { return StatusCode::NOT_FOUND.into_response(); }
    csv_download(&q.list, "radio-names", list_csv::write_radios(&st.db.radio_names(&q.list)))
}

fn csv_download(list: &str, what: &str, body: String) -> Response {
    let name = list_csv::file_name(list, what, chrono::Local::now().date_naive());
    ([(header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
      (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\""))], body).into_response()
}

/// Batch delete of talkgroups (`n` repeated): without `confirm=yes` it shows what will go; with it, deletes. Numbers not
/// in the list are ignored; when none is in it, back to the list without asking.
pub async fn delete_batch(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<Vec<(String, String)>>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsEdit) { return r; }
    let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if let Err(code) = check_csrf(&s, get("csrf").unwrap_or("")) { return code.into_response(); }
    let list = get("list").unwrap_or("");
    if !known(&st, list) { return StatusCode::BAD_REQUEST.into_response(); }
    let Ok(numbers) = f.iter().filter(|(k, _)| k == "n").map(|(_, v)| v.parse::<u16>()).collect::<Result<Vec<_>, _>>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let going: Vec<(String, String)> = st.db.talkgroups(list).into_iter().filter(|t| numbers.contains(&t.number))
        .map(|t| (t.number.to_string(), t.name)).collect();
    let home = url(list, "");
    if going.is_empty() { return Redirect::to(&home).into_response(); }
    if get("confirm") != Some("yes") {
        let ask = format!("Delete {} from {list}?", plural(going.len(), "talkgroup"));
        return confirm_delete(&s, list, &ask, "/talkgroups/delete", "n", "Talkgroup ID", &going, &home);
    }
    match st.db.delete_talkgroups(list, &numbers) {
        Ok(n) => changed(&st, &s, format!("deleted {} from {list}", plural(n, "talkgroup"))),
        Err(e) => return view(&st, &s, list, &TalkgroupForm::default(), Some(&format!("Could not delete: {e}"))).into_response(),
    }
    Redirect::to(&home).into_response()
}

/// Batch delete of radio names (`r` repeated), asked first as `delete_batch` does.
pub async fn delete_radios_batch(State(st): State<AppState>, Authed(s, _): Authed, Form(f): Form<Vec<(String, String)>>) -> Response {
    if let Some(r) = require(&st, &s, Function::TalkgroupsEdit) { return r; }
    let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if let Err(code) = check_csrf(&s, get("csrf").unwrap_or("")) { return code.into_response(); }
    let list = get("list").unwrap_or("");
    if !known(&st, list) { return StatusCode::BAD_REQUEST.into_response(); }
    let Ok(radios) = f.iter().filter(|(k, _)| k == "r").map(|(_, v)| v.parse::<u32>()).collect::<Result<Vec<_>, _>>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let going: Vec<(String, String)> = st.db.radio_names(list).into_iter().filter(|r| radios.contains(&r.radio))
        .map(|r| (r.radio.to_string(), r.name)).collect();
    let home = format!("{}#radios", url(list, ""));
    if going.is_empty() { return Redirect::to(&home).into_response(); }
    if get("confirm") != Some("yes") {
        let ask = format!("Delete {} from {list}?", plural(going.len(), "radio name"));
        return confirm_delete(&s, list, &ask, "/talkgroups/radios/delete", "r", "Radio ID", &going, &home);
    }
    match st.db.delete_radio_names(list, &radios) {
        Ok(n) => changed(&st, &s, format!("deleted {} from {list}", plural(n, "radio name"))),
        Err(e) => return view(&st, &s, list, &TalkgroupForm::default(), Some(&format!("Could not delete: {e}"))).into_response(),
    }
    Redirect::to(&home).into_response()
}

/// The confirmation page of a batch delete: `going` is (id, name) of each entry, sent again as `field` with `confirm=yes`.
#[allow(clippy::too_many_arguments)]
fn confirm_delete(s: &Session, list: &str, ask: &str, action: &str, field: &str, id_label: &str, going: &[(String, String)], cancel: &str) -> Response {
    layout("Talkgroups", Some(&s.user), &s.csrf, html! {
        h2 { (ask) }
        table {
            thead { tr { th { (id_label) } th { "Name" } } }
            tbody { @for (id, name) in going { tr { td { (id) } td { (name) } } } }
        }
        form method="post" action=(action) {
            (hidden(s, list))
            @for (id, _) in going { input type="hidden" name=(field) value=(id); }
            input type="hidden" name="confirm" value="yes";
            button.danger { "Delete" } " " a href=(cancel) { "Cancel" }
        }
    }).into_response()
}
