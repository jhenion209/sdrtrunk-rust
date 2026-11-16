//! A system's signal page, like the Java reference's channel signal quality view: live signal, decode quality and frequency error,
//! history charts (signal strength: average and observed range in dBFS; decode quality in percent) over a chosen
//! range, CSV export, and the control channel's demodulated symbols.
use crate::app::{AppState, Authed};
use crate::pages::layout::{layout_with, Head};
use crate::pages::live_view;
use crate::quality::{downsample, Summary};
use crate::receiver::{now_ms, SpectrumView};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use futures_core::Stream;
use maud::{html, Markup};
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

/// Chart ranges (the Java reference's): label and length.
const RANGES: [(&str, i64); 5] = [("1h", 3_600_000), ("6h", 21_600_000), ("24h", 86_400_000), ("7d", 604_800_000), ("30d", 2_592_000_000)];
const POINTS: usize = 300;
const W: f64 = 1000.0;
const H: f64 = 200.0;

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct RangeQuery { range: String }

fn range_ms(q: &RangeQuery) -> (&'static str, i64) {
    RANGES.iter().copied().find(|(l, _)| *l == q.range).unwrap_or(RANGES[0])
}

fn db_text(v: Option<f64>) -> String { v.map_or("–".into(), |d| format!("{d:.1} dBFS")) }
fn pct_text(v: Option<f64>) -> String { v.map_or("–".into(), |p| format!("{p:.0}%")) }

/// "+12 Hz (0.01 ppm)": a frequency error and what it is in PPM at `hz`.
pub fn error_text(error_hz: Option<f64>, hz: Option<u64>) -> String {
    match (error_hz, hz) {
        (Some(e), Some(f)) if f > 0 => format!("{e:+.0} Hz ({:+.2} ppm)", e / f as f64 * 1e6),
        (Some(e), _) => format!("{e:+.0} Hz"),
        _ => "not measured (no control channel decoded)".into(),
    }
}

/// "-30m" style label for a time `ms` before now.
fn ago(ms: i64) -> String {
    match ms / 60_000 {
        0 => "now".into(),
        m if m % 1440 == 0 => format!("-{}d", m / 1440),
        m if m % 60 == 0 => format!("-{}h", m / 60),
        m => format!("-{m}m"),
    }
}

struct Axis { from: i64, to: i64, lo: f64, hi: f64 }

impl Axis {
    fn x(&self, t: i64) -> f64 { (t - self.from) as f64 / (self.to - self.from) as f64 * W }
    fn y(&self, v: f64) -> f64 { H - (v.clamp(self.lo, self.hi) - self.lo) / (self.hi - self.lo) * H }
}

type Band<'a> = &'a dyn Fn(&Summary) -> Option<(f64, f64)>;

/// A line through `value` of each point (broken where samples are missing), an optional low/high band, a grid with
/// value labels every `step`, and time labels.
fn chart(points: &[(i64, Summary)], axis: &Axis, step: f64, unit: &str, value: impl Fn(&Summary) -> Option<f64>, band: Option<Band>) -> Markup {
    let slot = ((axis.to - axis.from) as f64 / POINTS as f64).max(60_000.0);
    let mut line = String::new();
    let mut prev: Option<i64> = None;
    for (t, s) in points {
        let Some(v) = value(s) else { prev = None; continue };
        let gap = prev.is_none_or(|p| (t - p) as f64 > 2.5 * slot);
        line.push_str(&format!("{}{:.1},{:.1} ", if gap { "M" } else { "L" }, axis.x(*t), axis.y(v)));
        prev = Some(*t);
    }
    let bar = (W / POINTS as f64).max(1.0);
    let span = axis.to - axis.from;
    let grid: Vec<f64> = (0..).map(|i| (axis.lo / step).ceil() * step + i as f64 * step).take_while(|v| *v <= axis.hi).collect();
    html! {
        svg.chart viewBox={ "0 0 " (W) " " (H + 20.0) } preserveAspectRatio="none" role="img" {
            @for v in &grid {
                line.grid x1="0" x2=(W) y1=(axis.y(*v)) y2=(axis.y(*v)) {}
                text.axis x="4" y=(axis.y(*v) - 3.0) { (v) " " (unit) }
            }
            @if let Some(band) = band {
                @for (t, s) in points { @if let Some((lo, hi)) = band(s) {
                    rect.band x=(axis.x(*t) - bar / 2.0) width=(bar) y=(axis.y(hi)) height=((axis.y(lo) - axis.y(hi)).max(1.0)) {}
                } }
            }
            path.trace d=(line) {}
            @for k in 0..=4i64 {
                text.axis x=(W * k as f64 / 4.0) y=(H + 15.0) text-anchor=(match k { 0 => "start", 4 => "end", _ => "middle" }) {
                    (ago(span - span * k / 4))
                }
            }
        }
    }
}

pub async fn page(State(st): State<AppState>, Authed(s, _): Authed, Path(id): Path<i64>, Query(q): Query<RangeQuery>) -> Response {
    let Some(sys) = st.db.system(id) else { return (StatusCode::NOT_FOUND, "No such system.").into_response() };
    let (label, span) = range_ms(&q);
    let now = now_ms();
    let samples = downsample(&st.db.quality(id, now - span), now - span, now, POINTS);
    let r = st.receivers.get(id);
    let live = r.as_ref().map(|r| r.quality.lock().unwrap_or_else(|p| p.into_inner()).summary(now, 30)).unwrap_or_default();
    let status = st.receivers.status(id).unwrap_or_default();
    let (lo, hi) = samples.iter().fold((f64::MAX, f64::MIN), |(lo, hi), (_, s)| {
        (s.signal_min.map_or(lo, |v| lo.min(v)), s.signal_max.map_or(hi, |v| hi.max(v)))
    });
    let (lo, hi) = if lo <= hi { ((lo / 10.0).floor() * 10.0 - 10.0, (hi / 10.0).ceil() * 10.0) } else { (-100.0, 0.0) };
    let signal_axis = Axis { from: now - span, to: now, lo, hi };
    let decode_axis = Axis { from: now - span, to: now, lo: 0.0, hi: 100.0 };
    let band = |s: &Summary| s.signal_min.zip(s.signal_max);
    let head = Head {
        subtitle: Some(html! { "Signal, decode quality and frequency error of the control channel." }),
        actions: Some(html! {
            @if let Some(t) = &status.tuner { a.button href={ "/tuners/" (t) "/spectrum" } { "Tuner spectrum" } }
            a.button.quiet href={ "/systems/" (id) } { "Settings" }
        }),
    };
    layout_with(&format!("{} signal", sys.label()), head, Some(&s.user), &s.csrf, html! {
        dl.facts {
            dt { "Receiver" } dd { (if r.is_some() { status.state.as_str() } else { "off" }) }
            dt { "Signal (30 s)" } dd { (db_text(live.signal_db))
                @if let (Some(a), Some(b)) = (live.signal_min, live.signal_max) { span.muted { " (" (format!("{a:.1} to {b:.1}")) ")" } } }
            dt { "Decode quality (30 s)" } dd { (pct_text(live.decode_pct())) }
            dt { "Frequency error" } dd { (error_text(status.freq_error_hz, status.control_hz)) }
            dt { "Tuner correction" } dd { (format!("{:.3} ppm", status.ppm)) @if status.auto_ppm { " (automatic)" } }
        }
        div.chart-head {
            h2 { "Signal history" }
            nav.ranges { @for (l, _) in RANGES {
                a href={ "?range=" (l) } aria-current=[(l == label).then_some("page")] { (l) }
            } }
            a href={ "/systems/" (id) "/signal.csv?range=" (label) } { "Export CSV" }
        }
        @if samples.is_empty() { p.muted { "No samples in the last " (label) " yet: one is stored each minute while the system runs." } } @else {
            h3 { "Signal strength " span.muted { "1 minute average and observed range · dBFS" } }
            (chart(&samples, &signal_axis, 10.0, "dB", |s| s.signal_db, Some(&band)))
            h3 { "Decode quality " span.muted { "successful frames · percent" } }
            (chart(&samples, &decode_axis, 25.0, "%", |s| s.decode_pct(), None))
        }
        h2 { "Channel" }
        p.muted { "The control channel's own samples, centred where it is decoded (after frequency correction). A signal "
            "sitting off centre is frequency error that correction has not taken out yet." }
        div.spectrum-controls {
            div.views role="group" aria-label="Channel view" {
                button #view-fft type="button" aria-pressed="true" { "FFT" }
                button #view-waterfall type="button" aria-pressed="false" { "Waterfall" }
            }
            button #hold type="button" { "Hold" }
            label.check { input #auto-range type="checkbox" checked; "Automatic range" }
            label { "Floor (dB)" input #floor type="number" step="5" value="-100"; }
            label { "Ceiling (dB)" input #ceiling type="number" step="5" value="-20"; }
        }
        dl.facts.readouts {
            dt { "Center" } dd #r-center { "–" }
            dt { "Peak" } dd #r-peak { "–" }
            dt { "Display range" } dd #r-range { "–" }
            dt { "Cursor" } dd #readout {}
        }
        p #spectrum-status .muted {}
        canvas #spectrum .spectrum width="1024" height="240" data-src={ "/systems/" (id) "/channel/events" } {}
        canvas #waterfall .spectrum width="1024" height="240" hidden {}
        h2 { "Symbols" }
        p.muted { "Demodulated control channel symbols, newest on the right. A clean signal shows four tight bands on the "
            "±1 and ±3 lines; spread bands mean noise or a weak signal." }
        p #symbols-status .muted {}
        canvas #symbols .spectrum width="1024" height="240" data-src={ "/systems/" (id) "/symbols/events" } {}
    }).into_response()
}

pub async fn csv(State(st): State<AppState>, Authed(_, _): Authed, Path(id): Path<i64>, Query(q): Query<RangeQuery>) -> Response {
    let Some(sys) = st.db.system(id) else { return (StatusCode::NOT_FOUND, "No such system.").into_response() };
    let (label, span) = range_ms(&q);
    let mut out = String::from("time_utc,signal_dbfs,signal_min_dbfs,signal_max_dbfs,decode_percent,valid_frames,attempted_frames\n");
    let f = |v: Option<f64>| v.map_or(String::new(), |v| format!("{v:.1}"));
    for (at, s) in st.db.quality(id, now_ms() - span) {
        let time = chrono::DateTime::from_timestamp_millis(at).map(|t| t.to_rfc3339()).unwrap_or_default();
        out.push_str(&format!("{time},{},{},{},{},{},{:.1}\n", f(s.signal_db), f(s.signal_min), f(s.signal_max), f(s.decode_pct()), s.valid, s.attempted));
    }
    let name: String = sys.label().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    ([(header::CONTENT_TYPE, "text/csv".to_string()),
      (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}-signal-{label}.csv\""))], out).into_response()
}

const NOT_RUNNING: &str = "The system is not running.";

fn running(id: i64) -> impl Fn(&AppState) -> Option<Arc<Mutex<SpectrumView>>> + Send + 'static {
    move |st| st.receivers.get(id).filter(|r| r.state() == "running").map(|r| r.spectrum.clone())
}

/// `symbols` events (about 10 a second) while the system runs, `idle` once a second otherwise.
pub async fn symbol_events(State(st): State<AppState>, Authed(_, token): Authed, Path(id): Path<i64>)
    -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    if st.db.system(id).is_none() { return Err(StatusCode::NOT_FOUND); }
    Ok(live_view(&st, token, "symbols", NOT_RUNNING, running(id), |v, until| {
        v.symbols_until_ms = until;
        v.symbols.as_ref().and_then(|f| Some((f.seq, serde_json::to_string(f).ok()?)))
    }))
}

/// The control channel's spectrum (the Java reference's per-channel FFT): `frame` events like the tuner spectrum's, about 10 a second
/// while the system runs, centred where the channel is decoded; `idle` once a second otherwise.
pub async fn channel_events(State(st): State<AppState>, Authed(_, token): Authed, Path(id): Path<i64>)
    -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    if st.db.system(id).is_none() { return Err(StatusCode::NOT_FOUND); }
    Ok(live_view(&st, token, "frame", NOT_RUNNING, running(id), |v, until| {
        v.channel_until_ms = until;
        v.channel.as_ref().and_then(|f| Some((f.seq, serde_json::to_string(f).ok()?)))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(error_text(Some(-12.4), Some(852_112_500)), "-12 Hz (-0.01 ppm)");
        assert_eq!(error_text(None, Some(1)), "not measured (no control channel decoded)");
        assert_eq!([ago(0), ago(900_000), ago(3_600_000), ago(2_592_000_000)], ["now", "-15m", "-1h", "-30d"]);
        assert_eq!(range_ms(&RangeQuery { range: "7d".into() }).1, 604_800_000);
        assert_eq!(range_ms(&RangeQuery { range: "bogus".into() }).0, "1h");
    }
}
