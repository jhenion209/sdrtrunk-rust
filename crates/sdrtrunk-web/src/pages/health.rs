//! `/health` and `/health/{name}` for Uptime Kuma: one line per enabled system, `200` when all are healthy, `503` when
//! any is not. No login (Kuma cannot sign in), so the lines name problems only: no tuner serials, settings or audio.
use crate::app::AppState;
use crate::health::{problems, Facts, FLAP_WINDOW_MS, QUALITY_WINDOW_MS};
use crate::receiver::now_ms;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

/// (system name, its problems) of every enabled system, or only the one named; sorted by name.
fn report(st: &AppState, only: Option<&str>) -> Vec<(String, Vec<String>)> {
    let now = now_ms();
    let connected = st.receivers.devices();
    let mut out: Vec<(String, Vec<String>)> = st.db.systems().into_iter()
        .filter(|s| s.enabled && only.is_none_or(|n| n == s.system_name))
        .map(|s| {
            let status = st.receivers.status(s.id);
            let quality: Vec<_> = st.db.quality(s.id, now - QUALITY_WINDOW_MS).into_iter().map(|(_, q)| q).collect();
            let facts = Facts { status: status.as_ref(), quality: &quality, connected: &connected,
                configured_tuner: sdrtrunk_source::normalize_tuner(&s.tuner), failures: st.receivers.failures(s.id, now - FLAP_WINDOW_MS) };
            (s.system_name.clone(), problems(now, &facts))
        }).collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn render(lines: &[(String, Vec<String>)]) -> Response {
    let healthy = lines.iter().all(|(_, p)| p.is_empty());
    let body: String = lines.iter().map(|(n, p)| format!("{n}: {}\n", if p.is_empty() { "OK".to_string() } else { p.join("; ") })).collect();
    let code = if healthy { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

pub async fn all(State(st): State<AppState>) -> Response {
    match tokio::task::spawn_blocking(move || report(&st, None)).await {
        Ok(lines) => render(&lines),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn one(State(st): State<AppState>, Path(name): Path<String>) -> Response {
    let wanted = name.clone();
    match tokio::task::spawn_blocking(move || report(&st, Some(&wanted))).await {
        Ok(lines) if lines.is_empty() => (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], format!("No enabled system named {name}.\n")).into_response(),
        Ok(lines) => render(&lines),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
