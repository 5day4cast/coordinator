//! The operator's Feedback pages: messages from the site's feedback form, newest first, and
//! one message with its status, a note, and links to the visitor's trace. Every message is
//! shown as escaped text; links in it are not made clickable.

use maud::{html, Markup};
use time::format_description::well_known::Rfc3339;

use crate::domain::feedback::{excerpt, FeedbackRow, FeedbackStatus};

/// Characters of a message the list shows.
const EXCERPT_CHARS: usize = 140;

fn when(row: &FeedbackRow) -> String {
    row.created_at.format(&Rfc3339).unwrap_or_default()
}

fn status_filter(selected: Option<FeedbackStatus>) -> Markup {
    html! {
        form.discovery-filters method="get" action="/admin/feedback" {
            label { "Status"
                select name="status" {
                    option value="all" selected[selected.is_none()] { "All" }
                    @for status in FeedbackStatus::ALL {
                        option value=(status.as_str()) selected[selected == Some(status)] { (status.as_str()) }
                    }
                }
            }
            button type="submit" { "Filter" }
        }
    }
}

/// The list. `rows` is `None` when the messages could not be read.
pub fn feedback_list(
    rows: Option<&[FeedbackRow]>,
    selected: Option<FeedbackStatus>,
    enabled: bool,
) -> Markup {
    html! { main.admin-workspace {
        p.eyebrow { "Players" } h1 { "Feedback" }
        @if !enabled {
            p.note { "The feedback form is off (" code { "[feedback].enabled" } "). Messages received earlier are listed below." }
        }
        (status_filter(selected))
        @match rows {
            None => p.notice { "Feedback messages are unavailable." },
            Some([]) => p { "No messages." },
            Some(rows) => {
                div.scroll { table.ops-table {
                    thead { tr { th { "Received (UTC)" } th { "Status" } th { "Page" } th { "Contact" } th { "Message" } } }
                    tbody {
                        @for row in rows {
                            tr {
                                td { a href=(format!("/admin/feedback/{}", row.id)) { (when(row)) } }
                                td { @if row.status == FeedbackStatus::New { strong { "new" } } @else { (row.status) } }
                                td { @if let Some(page) = &row.page { code { (page) } } }
                                td { (row.contact.as_deref().unwrap_or("")) }
                                td { a href=(format!("/admin/feedback/{}", row.id)) { (excerpt(&row.message, EXCERPT_CHARS)) } }
                            }
                        }
                    }
                } }
                p.note { "Showing " (rows.len()) ", newest first." }
            }
        }
    } }
}

/// The unread count beside Feedback in the navigation; empty at zero.
pub fn unread_badge(count: Option<i64>) -> Markup {
    html! {
        @if let Some(count) = count.filter(|count| *count > 0) {
            " " span.tag.is-warning aria-label=(format!("{count} unread")) { (count) }
        }
    }
}

fn trace_link(key: &str, value: &str) -> Markup {
    let mut url = reqwest::Url::parse("http://localhost/admin/visitors").expect("static URL");
    url.query_pairs_mut().append_pair(key, value);
    html! {
        a href=(format!("{}?{}", url.path(), url.query().unwrap_or_default())) { "Visitor trace" }
    }
}

/// One message. `saved` is said after a change was stored.
pub fn feedback_detail(row: &FeedbackRow, saved: bool) -> Markup {
    html! { main.admin-workspace {
        p.eyebrow { a href="/admin/feedback" { "Feedback" } } h1 { "Message " code { (row.id) } }
        (feedback_message(row))
        (feedback_status_form(row, saved))
    } }
}

fn feedback_message(row: &FeedbackRow) -> Markup {
    html! {
        // pre keeps the visitor's line breaks; maud escapes the text.
        pre.feedback-message { (row.message) }
        div.scroll { table.ops-table { tbody {
            tr { th { "Received (UTC)" } td { (when(row)) } }
            tr { th { "Contact" } td { (row.contact.as_deref().unwrap_or("none given")) } }
            tr { th { "Page" } td { @if let Some(page) = &row.page { code { (page) } } } }
            tr { th { "Request id" } td { @if let Some(rid) = &row.rid { code { (rid) } " " (trace_link("rid", rid)) } } }
            tr { th { "Session id" } td { @if let Some(sid) = &row.sid { code { (sid) } " " (trace_link("sid", sid)) } } }
            tr { th { "Address" } td { @if let Some(ip) = &row.ip { code { (ip) } " " (trace_link("ip", ip)) } } }
            tr { th { "Signed-in key" } td { @if let Some(pubkey) = &row.pubkey { code { (pubkey) } " " (trace_link("user", pubkey)) } @else { "not signed in" } } }
            tr { th { "Browser" } td { (row.user_agent.as_deref().unwrap_or("")) } }
            tr { th { "Alert" } td { @match row.notified_at { Some(at) => (at.format(&Rfc3339).unwrap_or_default()), None => "not sent" } } }
        } } }
    }
}

/// The status and note form. It posts with htmx, which sends the session's CSRF token.
pub fn feedback_status_form(row: &FeedbackRow, saved: bool) -> Markup {
    html! {
        form id="feedback-status" hx-post=(format!("/admin/feedback/{}", row.id)) hx-target="this" hx-swap="outerHTML" {
            div.form-grid {
                label { "Status"
                    select name="status" {
                        @for status in FeedbackStatus::ALL {
                            option value=(status.as_str()) selected[row.status == status] { (status.as_str()) }
                        }
                    }
                }
                label { "Operator note"
                    textarea name="note" rows="3" maxlength="2000" { (row.operator_note.as_deref().unwrap_or("")) }
                }
            }
            button type="submit" { "Save" }
            @if saved { " " span role="status" { "Saved." } }
        }
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;
    use uuid::Uuid;

    use super::*;

    fn row(message: &str) -> FeedbackRow {
        FeedbackRow {
            id: Uuid::parse_str("0199c1a2-0000-7000-8000-000000000001").unwrap(),
            created_at: datetime!(2026-10-07 10:00 UTC),
            message: message.into(),
            contact: Some("<b>me</b>".into()),
            page: Some("/entries".into()),
            rid: Some("0199c1a2-7b3e-7c11-9a00-5f1e2d3c4b5a".into()),
            sid: Some("Xq3vT9mPa1Lw0Zb8Yc7Rkd".into()),
            pubkey: Some("abcdef0123456789".into()),
            ip: Some("2001:db8::1".into()),
            user_agent: Some("Mozilla/5.0".into()),
            status: FeedbackStatus::New,
            operator_note: None,
            notified_at: None,
        }
    }

    #[test]
    fn messages_are_escaped_and_links_are_not_made() {
        let hostile = r#"<img src=x onerror=alert(1)> https://evil.example/"#;
        for html in [
            feedback_detail(&row(hostile), false).into_string(),
            feedback_list(Some(&[row(hostile)][..]), None, true).into_string(),
        ] {
            assert!(!html.contains("<img"), "{html}");
            assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
            assert!(!html.contains(r#"href="https://evil.example"#));
            assert!(!html.contains("<b>me</b>"));
        }
    }

    #[test]
    fn the_detail_links_to_the_visitor_trace() {
        let html = feedback_detail(&row("hi"), true).into_string();
        assert!(html.contains(r#"href="/admin/visitors?rid=0199c1a2-7b3e-7c11-9a00-5f1e2d3c4b5a""#));
        assert!(html.contains(r#"href="/admin/visitors?sid=Xq3vT9mPa1Lw0Zb8Yc7Rkd""#));
        assert!(html.contains(r#"href="/admin/visitors?ip=2001%3Adb8%3A%3A1""#));
        assert!(html.contains(r#"href="/admin/visitors?user=abcdef0123456789""#));
        assert!(html.contains(r#"hx-post="/admin/feedback/0199c1a2-0000-7000-8000-000000000001""#));
        assert!(html.contains("Saved."));
    }

    #[test]
    fn the_list_shows_an_excerpt_and_the_badge_hides_at_zero() {
        let long = "word ".repeat(100);
        let html =
            feedback_list(Some(&[row(&long)][..]), Some(FeedbackStatus::New), true).into_string();
        assert!(html.contains('…'));
        assert!(!html.contains(&long));
        assert!(html.contains(r#"<option value="new" selected>"#));
        assert_eq!(unread_badge(Some(0)).into_string(), "");
        assert_eq!(unread_badge(None).into_string(), "");
        assert!(unread_badge(Some(3)).into_string().contains(">3</span>"));
    }
}
