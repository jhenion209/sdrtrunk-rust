//! Router, shared state, login extractors and the server entry point.
use crate::auth::{new_setup_code, Function, Role, Session, Throttle};
use crate::db::Db;
use crate::pages;
use crate::receiver::{now_ms, Receivers, Sdrs};
use crate::https::WebStatus;
pub use crate::https::{own_certificate, serve_https};
use crate::web_config::{CertSource, Proxy, WebConfig};
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts};
use axum::http::{header, request::Parts, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub use crate::pages::layout::layout;

pub const COOKIE: &str = "sdrtrunk_rust_session";
/// The session cookie's name over HTTPS. A browser will not let plain HTTP replace a Secure cookie of the same name,
/// so the two have different names and signing in over plain HTTP on the same host still works. (`__Host-`: the
/// browser takes it only when Secure, for `Path=/`, without a domain.)
pub const SECURE_COOKIE: &str = "__Host-sdrtrunk_rust_session";
const MAX_AGE_S: i64 = crate::auth::MAX_MS / 1000;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub receivers: Arc<Receivers>,
    pub throttle: Arc<Throttle>,
    /// One-time first-run code; None once the owner exists.
    pub setup_code: Arc<Mutex<Option<String>>>,
    /// Reverse proxies whose `X-Forwarded-For` and `X-Forwarded-Proto` are believed (`SDRTRUNK_TRUSTED_PROXIES`).
    pub trusted: Arc<Vec<Proxy>>,
    /// Where the web UI listens and which certificate it serves (the Connected hosts page shows it).
    pub web: Arc<WebStatus>,
    /// Each session's open live pages.
    pub live: Arc<crate::live::LivePages>,
}

impl AppState {
    /// The session cookie; `Secure` when the request came over HTTPS (see `Secure`).
    pub fn session_cookie(&self, token: &str, secure: bool) -> String {
        if secure { format!("{SECURE_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={MAX_AGE_S}; Secure") }
        else { format!("{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={MAX_AGE_S}") }
    }
    pub fn clear_cookie(&self, secure: bool) -> String {
        if secure { format!("{SECURE_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0; Secure") }
        else { format!("{COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0") }
    }
}

/// Marks a request that arrived on the HTTPS listener (a request extension the HTTPS server adds).
#[derive(Clone, Copy, Debug)]
pub struct Https;

/// True when the visitor's connection is HTTPS: it arrived on the HTTPS listener, or a trusted proxy says
/// `X-Forwarded-Proto: https`. A cookie marked `Secure` over plain HTTP would never come back, so sign-in needs this.
pub struct Secure(pub bool);

impl FromRequestParts<AppState> for Secure {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        if parts.extensions.get::<Https>().is_some() { return Ok(Secure(true)); }
        let from_proxy = peer(parts).is_some_and(|p| st.trusted.iter().any(|t| t.contains(p)));
        let says_https = parts.headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"));
        Ok(Secure(from_proxy && says_https))
    }
}

/// The address the connection came from (None in tests that set none).
fn peer(parts: &Parts) -> Option<std::net::IpAddr> {
    parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_canonical())
}

/// The session token from the request's cookies: the HTTPS one (`SECURE_COOKIE`) when sent, else the plain one.
pub fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let named = |name: &str| headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(name).and_then(|r| r.strip_prefix('=')).map(str::to_string))
        .filter(|t| !t.is_empty());
    named(SECURE_COOKIE).or_else(|| named(COOKIE))
}

/// A signed-in user (any role). Without a valid session: 303 to /login.
pub struct Authed(pub Session, pub String);

impl FromRequestParts<AppState> for Authed {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Response> {
        let login = || Redirect::to("/login").into_response();
        let token = cookie_token(&parts.headers).ok_or_else(login)?;
        let Ok(ClientAddr(ip)) = ClientAddr::from_request_parts(parts, st).await;
        st.db.session(&token, now_ms(), &ip).map(|s| Authed(s, token)).ok_or_else(login)
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

/// The browser's `User-Agent` ("" when it sends none), kept with a new session.
pub struct Agent(pub String);

impl<S: Send + Sync> FromRequestParts<S> for Agent {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Agent(parts.headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string()))
    }
}

/// The visitor's address ("local" when unknown, e.g. in tests): the connection's, or behind a trusted proxy, the
/// one it forwards (`client::address`). Used for sign-in throttling and on the Connected hosts page.
pub struct ClientAddr(pub String);

impl FromRequestParts<AppState> for ClientAddr {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        Ok(ClientAddr(peer(parts).map(|p| crate::client::address(p, &parts.headers, &st.trusted).to_string())
            .unwrap_or_else(|| "local".into())))
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

/// Uploads of sdrtrunk playlists and RadioReference CSVs (axum's default is 2 MB; a large playlist is more).
const IMPORT_MAX: usize = 32 * 1024 * 1024;

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
        .route("/streams/import", post(pages::streams::import).layer(DefaultBodyLimit::max(IMPORT_MAX)))
        .route("/streams/import/apply", post(pages::streams::import_apply))
        .route("/streams/{id}", get(pages::streams::edit_page).post(pages::streams::save))
        .route("/streams/{id}/delete", post(pages::streams::delete))
        .route("/event-log", get(pages::event_log::page))
        .route("/recordings", get(pages::settings::recordings_page).post(pages::settings::recordings_submit))
        .route("/talkgroups", get(pages::talkgroups::page).post(pages::talkgroups::save))
        .route("/talkgroups/{number}/delete", post(pages::talkgroups::delete))
        .route("/talkgroups/unlisted", post(pages::talkgroups::unlisted))
        .route("/talkgroups/record", post(pages::talkgroups::record_batch))
        .route("/talkgroups/export.csv", get(pages::talkgroups::export_talkgroups))
        .route("/talkgroups/radios/export.csv", get(pages::talkgroups::export_radios))
        .route("/talkgroups/import", post(pages::talkgroups::import).layer(DefaultBodyLimit::max(IMPORT_MAX)))
        .route("/talkgroups/import/apply", post(pages::talkgroups::import_apply))
        .route("/talkgroups/import/file/apply", post(pages::talkgroups::import_file_apply))
        .route("/talkgroups/reviewed", post(pages::talkgroups::reviewed_all))
        .route("/talkgroups/{number}/reviewed", post(pages::talkgroups::reviewed_one))
        .route("/talkgroups/radios", post(pages::talkgroups::save_radio))
        .route("/talkgroups/radios/{radio}/delete", post(pages::talkgroups::delete_radio))
        .route("/talkgroups/radios/import", post(pages::talkgroups::import_radios).layer(DefaultBodyLimit::max(IMPORT_MAX)))
        .route("/talkgroups/radios/import/apply", post(pages::talkgroups::import_radios_apply))
        .route("/users", get(pages::users::page).post(pages::users::add))
        .route("/users/{id}/password", post(pages::users::reset_password))
        .route("/users/{id}/delete", post(pages::users::remove))
        .route("/users/{id}/allowed", post(pages::users::set_allowed))
        .route("/hosts", get(pages::hosts::page))
        .route("/hosts/{row}/end", post(pages::hosts::end))
        .route("/account", get(pages::account::page).post(pages::account::submit))
        .route("/login", get(pages::login::page).post(pages::login::submit))
        .route("/health", get(pages::health::all))
        .route("/health/{name}", get(pages::health::one))
        .route("/logout", post(pages::login::logout))
        .route("/setup", get(pages::setup::page).post(pages::setup::submit))
        .route("/app.css", get(|| async { ([(header::CONTENT_TYPE, "text/css")], include_str!("../assets/app.css")) }))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], include_str!("../assets/app.js")) }))
        .with_state(state)
}

/// Opens the database, starts the decoder with the saved settings and serves the UI (HTTPS, plain HTTP, or both, as
/// `web` says) until `signal`. The certificate and the listening addresses are settled first, so a bad setting or a
/// held port stops the start before any system is.
pub async fn serve(db_path: PathBuf, web: WebConfig, sdrs: Sdrs, signal: impl std::future::Future<Output = ()> + Send + 'static)
                   -> std::io::Result<()> {
    crate::https::provider();
    let other = std::io::Error::other;
    let data_dir = db_path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new(".")).to_path_buf();
    let (first, source, acme) = match (web.https, web.cert) {
        (None, _) => (None, "", None),
        (Some(_), CertSource::Own) => (Some(own_certificate(&data_dir).map_err(other)?), "own", None),
        (Some(_), CertSource::Files { cert, key }) => {
            crate::cert::check(&cert, &key).map_err(other)?;
            (Some(crate::cert::certified(&cert, &key).map_err(other)?), "yours", None)
        }
        // The own certificate is served until Let's Encrypt's arrives (or the one kept from before, at once).
        (Some(_), CertSource::Acme(a)) => (Some(own_certificate(&data_dir).map_err(other)?), "Let's Encrypt", Some(a)),
    };
    let certs = first.map(|c| Arc::new(crate::acme::Certs::new(Arc::new(c))));
    let bind = |addr: SocketAddr| {
        std::net::TcpListener::bind(addr).and_then(|l| l.set_nonblocking(true).map(|()| l))
            .map_err(|e| other(format!("cannot listen on {addr}: {e}")))
    };
    let https_listener = web.https.map(bind).transpose()?;
    let http_listener = web.http.map(bind).transpose()?;
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
        setup_code: Arc::new(Mutex::new(setup_code)), trusted: Arc::new(web.trusted),
        live: Arc::default(),
        web: Arc::new(WebStatus { https: web.https, http: web.http, source, certs: certs.clone(), renewal: Arc::default() }),
    };
    if let (Some(settings), Some(certs)) = (acme, &certs) {
        crate::acme::start(settings, data_dir.join("acme"), certs.clone(), state.web.renewal.clone(), std::future::ready(()));
    }
    if let Some(certs) = &certs {
        match crate::cert::found(&certs.current()) {
            Ok(f) => eprintln!("HTTPS certificate ({source}) for {} until {}", f.names.join(", "), f.not_after.format("%Y-%m-%d")),
            Err(e) => eprintln!("HTTPS certificate ({source}): {e}"),
        }
    }
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
    // One stop signal for both servers.
    let (stop, stopped) = tokio::sync::watch::channel(false);
    tokio::spawn(async move { signal.await; let _ = stop.send(true); });
    let stopped = move || { let mut rx = stopped.clone(); async move { let _ = rx.wait_for(|s| *s).await; } };
    let receivers = state.receivers.clone();
    let app = router(state);
    let https = async {
        let (Some(l), Some(certs)) = (https_listener, certs) else { return Ok(()) };
        eprintln!("SDRTrunk-Rust web UI on https://{}", l.local_addr()?);
        serve_https(l, app.clone(), certs, stopped(), SHUTDOWN_GRACE).await
    };
    let http = async {
        let Some(l) = http_listener else { return Ok(()) };
        eprintln!("SDRTrunk-Rust web UI on http://{}", l.local_addr()?);
        serve_until(tokio::net::TcpListener::from_std(l)?, app.clone(), stopped(), SHUTDOWN_GRACE).await
    };
    tokio::try_join!(https, http)?;
    // Stopping joins every decoder, so open recordings and call event log rows are written before the process exits.
    eprintln!("stopping systems");
    let _ = tokio::task::spawn_blocking(move || receivers.stop_all()).await;
    Ok(())
}

/// After the stop signal, how long open connections get to finish before serving ends anyway.
pub const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Serves `app` until `signal`, then lets open connections finish for at most `grace`: a dashboard's event stream
/// never finishes, and waiting for it kept the systems from stopping (systemd then killed the process, losing open
/// recordings and call event log rows).
pub async fn serve_until(listener: tokio::net::TcpListener, app: Router, signal: impl std::future::Future<Output = ()> + Send + 'static,
                         grace: std::time::Duration) -> std::io::Result<()> {
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let signalled = async move { signal.await; let _ = tx.send(true); };
    let server = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(signalled);
    tokio::select! {
        r = server => r,
        _ = async { if rx.wait_for(|s| *s).await.is_ok() { tokio::time::sleep(grace).await } else { std::future::pending().await } } => {
            eprintln!("connections still open {grace:?} after the stop signal (an open dashboard): stopping anyway");
            Ok(())
        }
    }
}

/// SIGTERM (systemctl stop/restart) or Ctrl-C.
pub async fn shutdown_signal() {
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
