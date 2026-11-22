//! Router, shared state, login extractors and the server entry point.
use crate::auth::{new_setup_code, Function, Role, Session, Throttle};
use crate::db::Db;
use crate::pages;
use crate::receiver::{now_ms, Receivers, Sdrs};
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::{header, request::Parts, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub use crate::pages::layout::layout;

pub const COOKIE: &str = "sdrtrunk_rust_session";
const MAX_AGE_S: i64 = crate::auth::MAX_MS / 1000;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub receivers: Arc<Receivers>,
    pub throttle: Arc<Throttle>,
    /// One-time first-run code; None once the owner exists.
    pub setup_code: Arc<Mutex<Option<String>>>,
    pub secure_cookies: bool,
}

impl AppState {
    pub fn session_cookie(&self, token: &str) -> String {
        format!("{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={MAX_AGE_S}{}", if self.secure_cookies { "; Secure" } else { "" })
    }
    pub fn clear_cookie(&self) -> String {
        format!("{COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{}", if self.secure_cookies { "; Secure" } else { "" })
    }
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(COOKIE).and_then(|r| r.strip_prefix('=')).map(str::to_string))
        .filter(|t| !t.is_empty())
}

/// A signed-in user (any role). Without a valid session: 303 to /login.
pub struct Authed(pub Session, pub String);

impl FromRequestParts<AppState> for Authed {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Response> {
        let login = || Redirect::to("/login").into_response();
        let token = cookie_token(&parts.headers).ok_or_else(login)?;
        st.db.session(&token, now_ms()).map(|s| Authed(s, token)).ok_or_else(login)
    }
}

/// The owner. Admins get 403.
pub struct OwnerOnly(pub Session);

impl FromRequestParts<AppState> for OwnerOnly {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Response> {
        let Authed(s, _) = Authed::from_request_parts(parts, st).await?;
        if s.user.role == Role::Owner { Ok(OwnerOnly(s)) } else { Err((StatusCode::FORBIDDEN, "Only the owner can do this.").into_response()) }
    }
}

/// Client address for throttling ("local" when unknown, e.g. in tests).
// ponytail: behind a reverse proxy every client shares the proxy's address; forwarded headers are not trusted.
pub struct ClientAddr(pub String);

impl FromRequestParts<AppState> for ClientAddr {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, _: &AppState) -> Result<Self, Self::Rejection> {
        Ok(ClientAddr(parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_string()).unwrap_or_else(|| "local".into())))
    }
}

/// The refusal (403 page) of a change the signed-in admin is not allowed; None when allowed (the owner always is).
pub fn require(st: &AppState, s: &Session, f: Function) -> Option<Response> {
    if st.db.may(&s.user, f) { return None; }
    let page = crate::pages::layout::layout("Not allowed", Some(&s.user), &s.csrf, maud::html! {
        p { "You are not allowed to " (f.label().to_lowercase()) ". Ask the owner." }
    });
    Some((StatusCode::FORBIDDEN, page).into_response())
}

pub fn check_csrf(session: &Session, form_csrf: &str) -> Result<(), StatusCode> {
    if !form_csrf.is_empty() && session.csrf == form_csrf { Ok(()) } else { Err(StatusCode::FORBIDDEN) }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(pages::status::page))
        .route("/events", get(pages::status::events))
        .route("/systems", get(pages::systems::list))
        .route("/systems/new", get(pages::systems::new_page).post(pages::systems::create))
        .route("/systems/{id}", get(pages::systems::edit_page).post(pages::systems::save))
        .route("/systems/{id}/delete", post(pages::systems::delete))
        .route("/systems/{id}/signal", get(pages::signal::page))
        .route("/systems/{id}/signal.csv", get(pages::signal::csv))
        .route("/systems/{id}/symbols/events", get(pages::signal::symbol_events))
        .route("/systems/{id}/channel/events", get(pages::signal::channel_events))
        .route("/tuners", get(pages::tuners::list))
        .route("/tuners/events", get(pages::tuners::events))
        .route("/tuners/{id}", get(pages::tuners::edit_page).post(pages::tuners::save))
        .route("/tuners/{id}/spectrum", get(pages::tuners::spectrum_page))
        .route("/tuners/{id}/spectrum/events", get(pages::tuners::spectrum_events))
        .route("/streams", get(pages::streams::list))
        .route("/streams/events", get(pages::streams::events))
        .route("/streams/new", get(pages::streams::new_page).post(pages::streams::create))
        .route("/streams/import", post(pages::streams::import))
        .route("/streams/import/apply", post(pages::streams::import_apply))
        .route("/streams/{id}", get(pages::streams::edit_page).post(pages::streams::save))
        .route("/streams/{id}/delete", post(pages::streams::delete))
        .route("/event-log", get(pages::event_log::page))
        .route("/recordings", get(pages::settings::recordings_page).post(pages::settings::recordings_submit))
        .route("/talkgroups", get(pages::talkgroups::page).post(pages::talkgroups::save))
        .route("/talkgroups/{number}/delete", post(pages::talkgroups::delete))
        .route("/talkgroups/unlisted", post(pages::talkgroups::unlisted))
        .route("/talkgroups/record", post(pages::talkgroups::record_batch))
        .route("/talkgroups/import", post(pages::talkgroups::import))
        .route("/talkgroups/import/apply", post(pages::talkgroups::import_apply))
        .route("/talkgroups/reviewed", post(pages::talkgroups::reviewed_all))
        .route("/talkgroups/{number}/reviewed", post(pages::talkgroups::reviewed_one))
        .route("/users", get(pages::users::page).post(pages::users::add))
        .route("/users/{id}/password", post(pages::users::reset_password))
        .route("/users/{id}/delete", post(pages::users::remove))
        .route("/account", get(pages::account::page).post(pages::account::submit))
        .route("/login", get(pages::login::page).post(pages::login::submit))
        .route("/logout", post(pages::login::logout))
        .route("/setup", get(pages::setup::page).post(pages::setup::submit))
        .route("/app.css", get(|| async { ([(header::CONTENT_TYPE, "text/css")], include_str!("../assets/app.css")) }))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], include_str!("../assets/app.js")) }))
        .with_state(state)
}

/// Opens the database, starts the decoder with the saved settings and serves the UI until the process ends.
pub async fn serve(db_path: PathBuf, listen: SocketAddr, secure_cookies: bool, sdrs: Sdrs) -> std::io::Result<()> {
    let db = Db::open(&db_path).map_err(|e| std::io::Error::other(format!("{}: {e}", db_path.display())))?;
    let setup_code = (db.user_count() == 0).then(|| {
        let c = new_setup_code();
        eprintln!("SDRTrunk-Rust first-run setup code: {c}  (open /setup in the browser)");
        c
    });
    // Settings saved on systems before the Tuners page move to the tuner each system uses (once).
    if let Err(e) = db.migrate_tuner_settings(&(sdrs.list)()) { eprintln!("moving tuner settings: {e}"); }
    let receivers = Receivers::new(sdrs);
    receivers.restart_all(&db);
    receivers.apply_streams(&db);
    let state = AppState {
        db: Arc::new(db), receivers: Arc::new(receivers), throttle: Arc::default(),
        setup_code: Arc::new(Mutex::new(setup_code)), secure_cookies,
    };
    // Once a minute: signal and decode quality history, and PPM moved by Automatic PPM.
    let (db, receivers) = (state.db.clone(), state.receivers.clone());
    tokio::spawn(async move {
        let mut minute = tokio::time::interval(std::time::Duration::from_secs(60));
        // A late tick (slow database or scan) waits a whole period again instead of firing a burst to catch up.
        minute.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        minute.tick().await;
        loop {
            minute.tick().await;
            let (db, receivers) = (db.clone(), receivers.clone());
            let _ = tokio::task::spawn_blocking(move || receivers.record_minute(&db, now_ms())).await;
        }
    });
    // Every 5 s: notice SDRs plugged in or removed.
    let (db, receivers) = (state.db.clone(), state.receivers.clone());
    tokio::spawn(async move {
        let mut scan = tokio::time::interval(std::time::Duration::from_millis(crate::receiver::Receivers::SCAN_MS));
        scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        scan.tick().await;
        loop {
            scan.tick().await;
            let (db, receivers) = (db.clone(), receivers.clone());
            let _ = tokio::task::spawn_blocking(move || receivers.rescan(&db)).await;
        }
    });
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!("SDRTrunk-Rust web UI on http://{listen}");
    let receivers = state.receivers.clone();
    axum::serve(listener, router(state).into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal()).await?;
    // Stopping joins every decoder, so open recordings and call event log rows are written before the process exits.
    eprintln!("stopping systems");
    let _ = tokio::task::spawn_blocking(move || receivers.stop_all()).await;
    Ok(())
}

/// SIGTERM (systemctl stop/restart) or Ctrl-C.
async fn shutdown_signal() {
    let ctrl_c = async { let _ = tokio::signal::ctrl_c().await; };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => { s.recv().await; }
            Err(e) => { eprintln!("cannot listen for SIGTERM: {e}"); std::future::pending::<()>().await }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}
