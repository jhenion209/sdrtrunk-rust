//! Dashboard: everything at a glance (a summary strip, one panel per system with its live readouts and the last
//! hour's decode quality, the live call feed, and the tuners) and the live event stream behind it.
use crate::auth::Function;
use crate::app::{AppState, Authed};
use crate::pages::layout::{layout_with, pill, Head};
use crate::db::TunerMode;
use crate::pages::tuners::{correction_text, rows as tuner_rows};
use crate::quality::{downsample, Summary};
use crate::receiver::{now_ms, CallEvent, Status};
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_core::Stream;
use maud::{html, Markup};
use serde::Serialize;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};
use tokio_stream::StreamExt;

fn mhz(hz: u64) -> String { format!("{:.4}", hz as f64 / 1e6) }

fn uptime(st: &Status) -> String {
    if st.started_ms == 0 { return "–".into(); }
    let s = (now_ms() - st.started_ms).max(0) / 1000;
    format!("{}h {:02}m {:02}s", s / 3600, s / 60 % 60, s % 60)
}

/// A system's status; a disabled or not yet started system shows as "off".
fn status_of(st: &AppState, id: i64) -> Status {
    st.receivers.status(id).unwrap_or_else(|| Status { state: "off".into(), ..Status::default() })
}

/// A system's readouts as shown (numbers only; units sit beside them): signal (dBFS) and decode quality (%) over the
/// last 30 s, measured frequency error (Hz) and the tuner correction in use.
#[derive(Serialize, Default)]
struct Readouts { signal: String, decode: String, freq: String, correction: String }

const NONE: &str = "–";

fn readouts_of(st: &AppState, id: i64, status: &Status) -> Readouts {
    let Some(r) = st.receivers.get(id) else {
        return Readouts { signal: NONE.into(), decode: NONE.into(), freq: NONE.into(), correction: NONE.into() };
    };
    let q = r.quality.lock().unwrap_or_else(|p| p.into_inner()).summary(now_ms(), 30);
    Readouts {
        signal: q.signal_db.map_or(NONE.into(), |d| format!("{d:.1}")),
        decode: q.decode_pct().map_or(NONE.into(), |p| format!("{p:.0}")),
        freq: status.freq_error_hz.map_or(NONE.into(), |e| format!("{e:+.0}")),
        correction: format!("{:.3} ppm{}", status.ppm, if status.auto_ppm { " (automatic)" } else { "" }),
    }
}

/// "none", or each helper tuner with where it sits: "Airspy 735863DC-2F67AFE3 at 862.5000 MHz".
fn helpers_text(status: &Status) -> String {
    if status.helpers.is_empty() { return "none".into(); }
    status.helpers.iter().map(|(t, hz)| format!("{} at {} MHz", sdrtrunk_source::tuner_label(t), mhz(*hz))).collect::<Vec<_>>().join(", ")
}

fn site_text(status: &Status) -> String {
    status.site.map_or("not decoded yet".into(), |(nac, wacn, system)| format!("NAC {nac:03X}, WACN {wacn:05X}, system {system:03X}"))
}

/// The last hour of decode quality as a small line (0 to 100%), or a note while history is collected.
fn trend(samples: &[(i64, Summary)], from: i64, to: i64) -> Markup {
    let points: Vec<String> = samples.iter().filter_map(|(t, s)| {
        let p = s.decode_pct()?;
        Some(format!("{:.1},{:.1}", (t - from) as f64 / (to - from) as f64 * 300.0, 40.0 - p * 0.38))
    }).collect();
    html! {
        @if points.len() < 2 { p.trend-empty { "Decode history appears after a few minutes." } } @else {
            svg.trend viewBox="0 0 300 40" preserveAspectRatio="none" role="img" aria-label="Decode quality, last hour" {
                line.trend-grid x1="0" x2="300" y1="21" y2="21" {}
                polyline points=(points.join(" ")) {}
            }
        }
    }
}

fn readout(id: &str, value: &str, unit: &str, label: &str) -> Markup {
    html! { div.gauge { span.value id=(id) { (value) } span.unit { (unit) } span.label { (label) } } }
}

fn system_panel(st: &AppState, sys: &crate::db::System, now: i64) -> Markup {
    let id = sys.id;
    let status = status_of(st, id);
    let r = readouts_of(st, id, &status);
    let samples = downsample(&st.db.quality(id, now - 3_600_000), now - 3_600_000, now, 60);
    html! {
        section.panel.system id={ "panel-" (id) } data-state=(status.state) {
            header.panel-head {
                div {
                    h2 { (sys.label()) }
                    p.sub { (sys.site_name) ", control " span id={ "control-" (id) } { (status.control_hz.map(|h| format!("{} MHz", mhz(h))).unwrap_or_default()) } }
                }
                (pill(&status.state, Some(format!("state-{id}"))))
            }
            p.error id={ "error-" (id) } { (status.error.as_deref().unwrap_or("")) }
            div.gauges {
                (readout(&format!("decode-{id}"), &r.decode, "%", "decode quality"))
                (readout(&format!("signal-{id}"), &r.signal, "dBFS", "signal"))
                (readout(&format!("rate-{id}"), &format!("{:.1}", status.tsbk_per_s), "msg/s", "control messages"))
                (readout(&format!("freq-{id}"), &r.freq, "Hz", "frequency error"))
            }
            (trend(&samples, now - 3_600_000, now))
            dl.details {
                dt { "Site" } dd id={ "site-" (id) } { (site_text(&status)) }
                dt { "Tuner" } dd id={ "tuner-" (id) } { (status.tuner.as_deref().map(sdrtrunk_source::tuner_label).unwrap_or_else(|| NONE.into())) }
                dt { "Helper tuners" } dd id={ "helpers-" (id) } { (helpers_text(&status)) }
                dt { "Correction" } dd id={ "correction-" (id) } { (r.correction) }
                dt { "Calls recorded" } dd id={ "recorded-" (id) } { (status.recorded) }
                dt { "Dropped samples" } dd id={ "dropped-" (id) } { (status.dropped) }
                dt { "Up for" } dd id={ "uptime-" (id) } { (uptime(&status)) }
            }
            p.error id={ "recording_error-" (id) } { (status.recording_error.as_deref().unwrap_or("")) }
            footer.panel-actions {
                a.button href={ "/systems/" (id) "/signal" } { "Signal" }
                @if let Some(t) = &status.tuner { a.button href={ "/tuners/" (t) "/spectrum" } { "Spectrum" } }
                a.button.quiet href={ "/systems/" (id) } { "Settings" }
            }
        }
    }
}

pub async fn page(State(st): State<AppState>, Authed(s, _): Authed) -> Markup {
    let systems = st.db.systems();
    let now = now_ms();
    let statuses: Vec<Status> = systems.iter().map(|sys| status_of(&st, sys.id)).collect();
    let running = statuses.iter().filter(|s| s.state == "running").count();
    let recorded: u64 = statuses.iter().map(|s| s.recorded).sum();
    let rate: f64 = statuses.iter().map(|s| s.tsbk_per_s).sum();
    let tuners = tuner_rows(&st);
    let head = Head {
        subtitle: Some(html! { "Live receivers, calls and tuners." }),
        actions: st.db.may(&s.user, Function::SystemsAdd).then(|| html! { a.button href="/systems/new" { "Add a system" } }),
    };
    layout_with("Dashboard", head, Some(&s.user), &s.csrf, html! {
        div.summary {
            div.stat { span.value #kpi-running { (running) " of " (systems.len()) } span.label { "systems receiving" } }
            div.stat { span.value #kpi-rate { (format!("{rate:.1}")) } span.label { "control messages per second" } }
            div.stat { span.value #kpi-recorded { (recorded) } span.label { "calls recorded since start" } }
            div.stat { span.value #kpi-calls { "0" } span.label { "calls seen on this page" } }
            @let streams: Vec<i64> = st.db.streams().into_iter().filter(|c| c.enabled).map(|c| c.id).collect();
            @if !streams.is_empty() {
                @let connected = streams.iter().filter(|&&id| st.receivers.streams.status(id).state == "connected").count();
                a.stat href="/streams" { span.value { (connected) " of " (streams.len()) } span.label { "streams connected" } }
            }
        }
        // Systems and tuners share the left column, so a long call feed on the right never pushes the tuners down.
        div.board {
            div.board-main {
                div.systems {
                    @if systems.is_empty() {
                        section.panel.empty { h2 { "No systems yet" } p { "Add a system to start receiving: its control channel, "
                            "modulation and which SDR to use." } @if st.db.may(&s.user, Function::SystemsAdd) { a.button href="/systems/new" { "Add a system" } } }
                    }
                    @for sys in &systems { (system_panel(&st, sys, now)) }
                }
                section.tuners aria-labelledby="tuners-title" {
                    h2 #tuners-title { "Tuners" }
                    @if tuners.is_empty() { p.muted { "No SDR found. Plug in an Airspy or RTL-SDR." } }
                    div.tiles { @for t in &tuners {
                        a.tile href={ "/tuners/" (t.id) } data-state=(if t.config.mode == TunerMode::Disabled { "off" } else if t.user.is_some() { "running" } else { "idle" }) {
                            strong { (sdrtrunk_source::tuner_label(&t.id)) }
                            span { @match &t.user { Some((label, _)) => { (label) } None => { (match t.config.mode { TunerMode::Live => "idle", TunerMode::Setup => "setup", TunerMode::Disabled => "disabled" }) } } }
                            span.muted { (correction_text(t)) @if !t.connected { ", not connected" } }
                        }
                    } }
                }
            }
            section.panel.feed aria-labelledby="feed-title" {
                header.panel-head { h2 #feed-title { "Live calls" } }
                ol #calls { li #nocalls .muted { "Waiting for calls…" } }
            }
        }
    })
}

#[derive(Serialize)]
struct CallJson<'a> { #[serde(flatten)] call: &'a CallEvent, name: Option<String>, radio_name: Option<String> }

#[derive(Serialize)]
struct SystemStatusJson { id: i64, #[serde(flatten)] s: Status, uptime: String, site_text: String, helpers_text: String, #[serde(flatten)] readouts: Readouts }

/// `status` every second (an array, one entry per system), `call` per grant. A slow page skips missed calls instead
/// of stalling anything.
pub async fn events(State(st): State<AppState>, Authed(_, token): Authed) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let db = st.db.clone();
    let calls = BroadcastStream::new(st.receivers.calls.subscribe()).filter_map(move |r| {
        let call = r.ok()?;
        let name = db.talkgroups(&call.alias_list).into_iter().find(|t| t.number == call.group).map(|t| t.name);
        let radio_name = db.radio_names(&call.alias_list).into_iter().find(|r| r.radio == call.source).map(|r| r.name);
        Event::default().event("call").json_data(CallJson { call: &call, name, radio_name }).ok()
    });
    let state = st.clone();
    let ticks = IntervalStream::new(tokio::time::interval(Duration::from_secs(1))).filter_map(move |_| {
        let all: Vec<SystemStatusJson> = state.db.systems().iter().map(|sys| {
            let s = status_of(&state, sys.id);
            SystemStatusJson { id: sys.id, uptime: uptime(&s), site_text: site_text(&s), helpers_text: helpers_text(&s), readouts: readouts_of(&state, sys.id, &s), s }
        }).collect();
        Event::default().event("status").json_data(all).ok()
    });
    // The stream ends as soon as the session does (sign out, password change, user removed, expiry).
    let db = st.db.clone();
    let live = calls.merge(ticks).take_while(move |_| db.session_valid(&token, now_ms()));
    Sse::new(live.map(Ok)).keep_alive(KeepAlive::default())
}
