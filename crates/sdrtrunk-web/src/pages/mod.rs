//! Page handlers. Each page is plain server-rendered HTML.
pub mod account;
pub mod event_log;
pub mod health;
pub mod layout;
pub mod users;
pub mod login;
pub mod settings;
pub mod signal;
pub mod setup;
pub mod status;
pub mod streams;
pub mod systems;
pub mod talkgroups;
pub mod tuners;

use crate::app::AppState;
use crate::receiver::{now_ms, SpectrumView};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_core::Stream;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_stream::wrappers::IntervalStream;
use tokio_stream::StreamExt;

/// A live part of a page (the Streams and Tuners tables): every second `render` draws it, and a drawing that changed
/// goes out as an `html` event for the page's script to swap in. `render` is the same function the page draws it with
/// (maud escapes every value), so the markup is safe to insert. The stream ends as soon as the session does.
pub fn live_html<R>(st: &AppState, token: String, render: R) -> Sse<impl Stream<Item = Result<Event, Infallible>>>
where
    R: Fn(&AppState) -> String + Send + 'static,
{
    let state = st.clone();
    let mut last = String::new();
    let ticks = IntervalStream::new(tokio::time::interval(Duration::from_secs(1))).filter_map(move |_| {
        let html = render(&state);
        if html == last { return None; }
        last.clone_from(&html);
        Some(Event::default().event("html").data(html))
    });
    let db = st.db.clone();
    let open = st.live.open(crate::auth::token_hash(&token));
    let live = ticks.take_while(move |_| { let _counted = &open; db.session_valid(&token, now_ms()) });
    Sse::new(live.map(Ok)).keep_alive(KeepAlive::default())
}

/// A live display of a receiver (tuner spectrum, channel spectrum, symbols) as Server-Sent Events. Every 100 ms,
/// `watch` marks the display watched until the given time (the decoder only does the work while someone watches,
/// and stops two seconds after the last viewer leaves) and returns its latest frame as (sequence number, JSON); a new
/// frame goes out as an `event` event. While `find` gives no display, `idle` goes out once a second. The stream ends
/// as soon as the session does (sign out, password change, user removed, expiry).
pub fn live_view<F, W>(st: &AppState, token: String, event: &'static str, idle: &'static str, find: F, watch: W)
    -> Sse<impl Stream<Item = Result<Event, Infallible>>>
where
    F: Fn(&AppState) -> Option<Arc<Mutex<SpectrumView>>> + Send + 'static,
    W: Fn(&mut SpectrumView, i64) -> Option<(u64, String)> + Send + 'static,
{
    let (sent, ticks) = (AtomicU64::new(0), AtomicU64::new(0));
    let state = st.clone();
    let frames = IntervalStream::new(tokio::time::interval(Duration::from_millis(100))).filter_map(move |_| {
        let tick = ticks.fetch_add(1, Ordering::Relaxed);
        let Some(view) = find(&state) else { return tick.is_multiple_of(10).then(|| Event::default().event("idle").data(idle)) };
        let (seq, json) = watch(&mut view.lock().unwrap_or_else(|p| p.into_inner()), now_ms() + 2000)?;
        (sent.swap(seq, Ordering::Relaxed) != seq).then(|| Event::default().event(event).data(json))
    });
    let db = st.db.clone();
    let open = st.live.open(crate::auth::token_hash(&token));
    let live = frames.take_while(move |_| { let _counted = &open; db.session_valid(&token, now_ms()) });
    Sse::new(live.map(Ok)).keep_alive(KeepAlive::default())
}
