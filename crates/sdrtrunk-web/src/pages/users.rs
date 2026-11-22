//! Owner only: admins (add, reset password, remove) and the audit log.
use crate::app::{check_csrf, AppState, OwnerOnly};
use crate::auth::{Function, Role, Session};
use crate::pages::layout::{layout, notice};
use crate::receiver::now_ms;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use maud::{html, Markup};
use serde::Deserialize;

/// Epoch ms -> "2026-11-13 17:59:03 UTC" (civil-from-days, no date library needed).
pub fn utc(ms: i64) -> String {
    let (days, secs) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000) / 1000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", secs / 3600, secs / 60 % 60, secs % 60)
}

fn view(st: &AppState, s: &Session, error: Option<&str>) -> Markup {
    let users = st.db.users();
    let log = st.db.audit_log(200);
    layout("Users", Some(&s.user), &s.csrf, html! {
        (notice(error, None))
        table {
            thead { tr { th { "User" } th { "Role" } th { "Reset password" } th {} } }
            tbody { @for u in &users { tr {
                td { (u.username) }
                td { @if u.role == Role::Owner { "owner" } @else { "admin" } }
                @if u.role == Role::Owner {
                    td.muted { "Use Account" } td {}
                } @else {
                    td { form method="post" action={ "/users/" (u.id) "/password" } {
                        input type="hidden" name="csrf" value=(s.csrf);
                        input name="password" type="password" autocomplete="new-password" placeholder="New password" required;
                        button.plain { "Set" }
                    } }
                    td { form method="post" action={ "/users/" (u.id) "/delete" } {
                        input type="hidden" name="csrf" value=(s.csrf);
                        button.danger { "Remove" }
                    } }
                }
            }
            @if u.role != Role::Owner {
                @let allowed = st.db.allowed(u.id);
                tr { td colspan="4" { form method="post" action={ "/users/" (u.id) "/allowed" } {
                    input type="hidden" name="csrf" value=(s.csrf);
                    @for f in Function::ALL {
                        label { input type="checkbox" name="function" value=(f.id()) checked[allowed.contains(&f)]; " " (f.label()) }
                    }
                    button.plain { "Save permissions" }
                } } }
            } } }
        }
        h2 { "Add an admin" }
        p.muted { "New admins can change nothing until you allow it below their name." }
        form method="post" action="/users" {
            input type="hidden" name="csrf" value=(s.csrf);
            label { "Username" input name="username" autocomplete="off" required; }
            label { "Password (12 characters or more)" input name="password" type="password" autocomplete="new-password" required; }
            button { "Add admin" }
        }
        h2 { "Audit log" }
        @if log.is_empty() { p.muted { "Nothing yet." } } @else {
            table {
                thead { tr { th { "When" } th { "Who" } th { "What" } } }
                tbody { @for a in &log { tr { td { (utc(a.at)) } td { (a.username) } td { (a.action) } } } }
            }
        }
    })
}

pub async fn page(State(st): State<AppState>, OwnerOnly(s): OwnerOnly) -> Markup { view(&st, &s, None) }

#[derive(Deserialize)]
pub struct AddForm { #[serde(default)] csrf: String, username: String, password: String }

pub async fn add(State(st): State<AppState>, OwnerOnly(s): OwnerOnly, Form(f): Form<AddForm>) -> Response {
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    let name = f.username.trim();
    match st.db.create_user(name, &f.password, Role::Admin, now_ms()) {
        Ok(_) => {
            st.db.audit(now_ms(), &s.user.username, &format!("added admin {name}"));
            Redirect::to("/users").into_response()
        }
        Err(e) => view(&st, &s, Some(&e.to_string())).into_response(),
    }
}

fn name_of(st: &AppState, id: i64) -> Option<(String, Role)> {
    st.db.users().into_iter().find(|u| u.id == id).map(|u| (u.username, u.role))
}

#[derive(Deserialize)]
pub struct PasswordForm { #[serde(default)] csrf: String, password: String }

pub async fn reset_password(State(st): State<AppState>, OwnerOnly(s): OwnerOnly, Path(id): Path<i64>, Form(f): Form<PasswordForm>) -> Response {
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    let Some((name, role)) = name_of(&st, id) else { return view(&st, &s, Some("No such user.")).into_response() };
    if role == Role::Owner { return view(&st, &s, Some("Use Account to change your own password.")).into_response(); }
    match st.db.set_password(id, &f.password) {
        Ok(()) => {
            st.db.audit(now_ms(), &s.user.username, &format!("reset password for {name}"));
            Redirect::to("/users").into_response()
        }
        Err(e) => view(&st, &s, Some(&e.to_string())).into_response(),
    }
}

/// Saves what admin `id` may change: the checked boxes arrive as repeated `function=<id>` pairs (unknown ids ignored).
pub async fn set_allowed(State(st): State<AppState>, OwnerOnly(s): OwnerOnly, Path(id): Path<i64>, Form(pairs): Form<Vec<(String, String)>>) -> Response {
    let csrf = pairs.iter().find(|(k, _)| k == "csrf").map(|(_, v)| v.as_str()).unwrap_or("");
    if let Err(code) = check_csrf(&s, csrf) { return code.into_response(); }
    let Some((name, role)) = name_of(&st, id) else { return view(&st, &s, Some("No such user.")).into_response() };
    if role == Role::Owner { return view(&st, &s, Some("The owner can always do everything.")).into_response(); }
    let fs: Vec<Function> = pairs.iter().filter(|(k, _)| k == "function").filter_map(|(_, v)| Function::parse(v)).collect();
    match st.db.set_allowed(id, &fs) {
        Ok(()) => {
            st.db.audit(now_ms(), &s.user.username, &format!("changed permissions of {name}"));
            Redirect::to("/users").into_response()
        }
        Err(e) => view(&st, &s, Some(&e.to_string())).into_response(),
    }
}

#[derive(Deserialize)]
pub struct CsrfOnly { #[serde(default)] csrf: String }

pub async fn remove(State(st): State<AppState>, OwnerOnly(s): OwnerOnly, Path(id): Path<i64>, Form(f): Form<CsrfOnly>) -> Response {
    if let Err(code) = check_csrf(&s, &f.csrf) { return code.into_response(); }
    let name = name_of(&st, id).map(|(n, _)| n).unwrap_or_default();
    match st.db.remove_user(id) {
        Ok(()) => {
            st.db.audit(now_ms(), &s.user.username, &format!("removed admin {name}"));
            Redirect::to("/users").into_response()
        }
        Err(e) => view(&st, &s, Some(&e.to_string())).into_response(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn utc_formats_epoch_ms() {
        assert_eq!(super::utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(super::utc(1_794_592_743_000), "2026-11-13 17:59:03 UTC");
        assert_eq!(super::utc(951_782_400_000), "2000-02-29 00:00:00 UTC");
    }
}
