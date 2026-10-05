//! Bounded list selection. Full records are loaded only for the selected page.
use super::CompetitionStore;
use sqlx::{QueryBuilder, Sqlite};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct ListPage {
    pub ids: Vec<Uuid>,
    pub before: Option<Uuid>,
    pub since: Option<OffsetDateTime>,
    pub status: Option<String>,
    pub history: bool,
    pub limit: usize,
}

impl ListPage {
    fn conditions<'a>(&self, query: &mut QueryBuilder<'a, Sqlite>, id: &str, updated: &str) {
        if !self.ids.is_empty() {
            query.push(format!(" AND {id} IN ("));
            let mut values = query.separated(",");
            for id in &self.ids {
                values.push_bind(id.to_string());
            }
            values.push_unseparated(")");
        }
        if let Some(before) = self.before {
            query
                .push(format!(" AND {id} < "))
                .push_bind(before.to_string());
        }
        if let Some(since) = self.since {
            query
                .push(format!(" AND julianday({updated}) >= julianday("))
                .push_bind(since.unix_timestamp())
                .push(", 'unixepoch')");
        }
        let terminal = "(c.completed_at IS NOT NULL OR c.cancelled_at IS NOT NULL OR c.failed_at IS NOT NULL OR c.pools_finished_at IS NOT NULL)";
        match self.status.as_deref() {
            Some("active") => {
                query.push(format!(" AND NOT {terminal}"));
            }
            Some("finished") => {
                query.push(format!(" AND {terminal}"));
            }
            Some("failed") => {
                query.push(" AND c.failed_at IS NOT NULL");
            }
            Some("cancelled") => {
                query.push(" AND c.cancelled_at IS NOT NULL");
            }
            Some("open") => {
                query.push(format!(" AND NOT {terminal} AND c.pools_formed_at IS NULL AND julianday(json_extract(c.event_submission, '$.start_observation_date')) > julianday('now')"));
            }
            Some("live") => {
                query.push(format!(" AND NOT {terminal} AND julianday(json_extract(c.event_submission, '$.start_observation_date')) <= julianday('now') AND julianday(json_extract(c.event_submission, '$.end_observation_date')) > julianday('now')"));
            }
            Some("awaiting") => {
                query.push(format!(" AND NOT {terminal} AND c.attestation IS NULL AND c.expiry_broadcasted_at IS NULL AND c.awaiting_attestation_at IS NOT NULL"));
            }
            _ if !self.history && self.ids.is_empty() && self.since.is_none() => {
                query.push(format!(" AND (NOT {terminal} OR julianday(cu.updated_at) >= julianday('now', '-14 days'))"));
            }
            _ => {}
        }
    }
}

impl CompetitionStore {
    pub async fn competition_page_ids(&self, page: &ListPage) -> Result<Vec<Uuid>, sqlx::Error> {
        let mut query = QueryBuilder::<Sqlite>::new("SELECT c.id FROM competitions c JOIN list_updates cu ON cu.kind='competition' AND cu.id=c.id WHERE 1=1");
        page.conditions(&mut query, "c.id", "cu.updated_at");
        query
            .push(" ORDER BY c.id DESC LIMIT ")
            .push_bind((page.limit + 1) as i64);
        let ids: Vec<String> = query
            .build_query_scalar()
            .fetch_all(self.db_connection.read())
            .await?;
        ids.into_iter()
            .map(|id| Uuid::parse_str(&id).map_err(|e| sqlx::Error::Decode(Box::new(e))))
            .collect()
    }

    pub async fn entry_page_ids(
        &self,
        pubkey: &str,
        events: &[Uuid],
        page: &ListPage,
    ) -> Result<Vec<Uuid>, sqlx::Error> {
        let mut query = QueryBuilder::<Sqlite>::new("SELECT e.id FROM entries e JOIN competitions c ON c.id=e.event_id JOIN list_updates cu ON cu.kind='competition' AND cu.id=c.id JOIN list_updates eu ON eu.kind='entry' AND eu.id=e.id WHERE e.pubkey = ");
        query.push_bind(pubkey.to_owned());
        if !events.is_empty() {
            query.push(" AND e.event_id IN (");
            let mut values = query.separated(",");
            for id in events {
                values.push_bind(id.to_string());
            }
            values.push_unseparated(")");
        }
        page.conditions(&mut query, "e.id", "eu.updated_at");
        query
            .push(" ORDER BY e.id DESC LIMIT ")
            .push_bind((page.limit + 1) as i64);
        let ids: Vec<String> = query
            .build_query_scalar()
            .fetch_all(self.db_connection.read())
            .await?;
        ids.into_iter()
            .map(|id| Uuid::parse_str(&id).map_err(|e| sqlx::Error::Decode(Box::new(e))))
            .collect()
    }
}
