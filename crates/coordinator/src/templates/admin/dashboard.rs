use maud::{html, Markup};
use uuid::Uuid;

pub fn competition_success(competition_id: &Uuid) -> Markup {
    html! {
        div class="notification is-success" {
            "Competition created. " a href=(format!("/admin/operations/{competition_id}")) { "Inspect competition" }
        }
        input type="hidden" name="id" id="competitionIdInput" value=(Uuid::now_v7()) hx-swap-oob="true";
    }
}

/// Error notification fragment
pub fn competition_error(message: &str) -> Markup {
    html! {
        div class="notification is-danger" {
            "Failed to create competition: " (message)
        }
    }
}

/// Success message notification fragment (generic)
pub fn competition_success_message(message: &str) -> Markup {
    html! {
        div class="notification is-success" {
            (message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_created_competition_gives_the_form_a_new_id() {
        let created = Uuid::now_v7();
        let html = competition_success(&created).into_string();
        assert!(html.contains(r#"id="competitionIdInput""#));
        assert!(html.contains(r#"hx-swap-oob="true""#));
        assert_eq!(
            html.matches(&created.to_string()).count(),
            1,
            "only the notification may name the created ID; the next submit needs a new one"
        );
    }
}
