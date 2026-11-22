//! Page frame: a sidebar grouped by what the operator is doing (monitoring, configuring, administering), a page
//! header with the page's actions, and small shared pieces.
use crate::auth::{Role, User};
use crate::db::FieldError;
use maud::{html, Markup, DOCTYPE};

/// The page header's optional parts: a line under the title and the page's actions (buttons and links).
#[derive(Default)]
pub struct Head { pub subtitle: Option<Markup>, pub actions: Option<Markup> }

pub fn layout(title: &str, user: Option<&User>, csrf: &str, body: Markup) -> Markup {
    layout_with(title, Head::default(), user, csrf, body)
}

pub fn layout_with(title: &str, head: Head, user: Option<&User>, csrf: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="dark light";
                title { (title) " · SDRTrunk-Rust" }
                link rel="stylesheet" href="/app.css";
            }
            body.shell[user.is_some()] {
                @if let Some(u) = user {
                    aside.sidebar {
                        a.brand href="/" { span.mark aria-hidden="true" {} span { "SDRTrunk-Rust" } small { "P25 recorder" } }
                        nav.side aria-label="Main" {
                            p.group { "Monitor" }
                            a href="/" data-match="/" { "Dashboard" }
                            p.group { "Configure" }
                            a href="/systems" data-match="/systems" { "Systems" }
                            a href="/tuners" data-match="/tuners" { "Tuners" }
                            a href="/talkgroups" data-match="/talkgroups" { "Talkgroups" }
                            a href="/streams" data-match="/streams" { "Streams" }
                            a href="/recordings" data-match="/recordings" { "Recordings" }
                            a href="/event-log" data-match="/event-log" { "Event Log" }
                            p.group { "Admin" }
                            @if u.role == Role::Owner { a href="/users" data-match="/users" { "Users" } }
                            a href="/account" data-match="/account" { "Account" }
                        }
                        form.who method="post" action="/logout" {
                            input type="hidden" name="csrf" value=(csrf);
                            span { (u.username) }
                            button.link { "Sign out" }
                        }
                    }
                }
                div.content {
                    header.page-head {
                        div {
                            h1 { (title) }
                            @if let Some(sub) = head.subtitle { p.sub { (sub) } }
                        }
                        @if let Some(actions) = head.actions { div.actions { (actions) } }
                    }
                    main { (body) }
                }
                script src="/app.js" defer {}
            }
        }
    }
}

/// A state as a coloured pill: running, starting, error, stopped or off.
pub fn pill(state: &str, id: Option<String>) -> Markup {
    html! { span class={ "pill state-" (state) } id=[id] { (state) } }
}

/// Shown above a form the signed-in admin may not submit.
pub fn read_only(f: crate::auth::Function) -> Markup {
    html! { p.muted { "Read only: ask the owner to allow " (f.label().to_lowercase()) "." } }
}

/// Error text for one form field, if any.
pub fn field_error(errors: &[FieldError], field: &str) -> Markup {
    html! { @for e in errors.iter().filter(|e| e.field == field) { p.error { (e.message) } } }
}

/// One-line message above a form: error or success.
pub fn notice(error: Option<&str>, ok: Option<&str>) -> Markup {
    html! {
        @if let Some(e) = error { p.error { (e) } }
        @if let Some(m) = ok { p.ok { (m) } }
    }
}
