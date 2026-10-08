use crate::domain::mainnet_signup::SignupRow;
use maud::{html, Markup};

pub fn mainnet_signups(
    rows: &[SignupRow],
    total: i64,
    page: u32,
    page_size: i64,
    enabled: bool,
) -> Markup {
    html! { main.admin-workspace {
        p.eyebrow { "Players" } h1 { "Mainnet signups" }
        p.note { "Total signups: " (total) ". Emails will be used for the mainnet launch announcement." }
        @if !enabled { p.notice { "The signup form is closed. Previously collected addresses are available below." } }
        p { a href="/admin/mainnet-signups.csv" download { "Download CSV" } }
        @if rows.is_empty() { p.notice { "No signups on this page." } }
        @else {
            div.scroll { table.ops-table {
                thead { tr { th { "Email address" } th { "Signed up (UTC)" } } }
                tbody { @for row in rows { tr { td { (row.email) } td { (row.created_at) } } } }
            } }
        }
        nav aria-label="Signup pages" {
            @if page > 0 { a href=(format!("/admin/mainnet-signups?page={}", page - 1)) { "Previous" } " " }
            @if (i64::from(page) + 1) * page_size < total {
                a href=(format!("/admin/mainnet-signups?page={}", page + 1)) { "Next" }
            }
        }
    } }
}
