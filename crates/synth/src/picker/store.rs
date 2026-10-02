//! Each pick, saved with its competition: for the run's page, and so a lane does not pick the
//! same stations again too soon.

use std::collections::HashSet;

use anyhow::Result;
use sqlx::SqlitePool;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use super::Pick;
use crate::db::SynthDb;

pub(crate) async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS station_picks (
            competition_id TEXT PRIMARY KEY,
            lane TEXT NOT NULL,
            stations_json TEXT NOT NULL,
            pick_json TEXT NOT NULL,
            picked_at TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS station_picks_by_lane ON station_picks (lane)")
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn record(db: &SynthDb, pick: &Pick) -> Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO station_picks \
         (competition_id, lane, stations_json, pick_json, picked_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(pick.competition_id.to_string())
    .bind(&pick.lane)
    .bind(serde_json::to_string(&pick.stations())?)
    .bind(serde_json::to_string(pick)?)
    .bind(time::OffsetDateTime::now_utc().format(&Rfc3339)?)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// The stations picked for `lane`'s last `runs` competitions.
pub async fn recent_stations(db: &SynthDb, lane: &str, runs: usize) -> Result<HashSet<String>> {
    if runs == 0 {
        return Ok(HashSet::new());
    }
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT stations_json FROM station_picks WHERE lane = ? \
         ORDER BY rowid DESC LIMIT ?",
    )
    .bind(lane)
    .bind(i64::try_from(runs)?)
    .fetch_all(db.pool())
    .await?;
    let mut stations = HashSet::new();
    for row in rows {
        stations.extend(serde_json::from_str::<Vec<String>>(&row)?);
    }
    Ok(stations)
}

/// The pick for a competition, if its stations were picked for the weather.
pub async fn pick_for(db: &SynthDb, competition_id: Uuid) -> Option<Pick> {
    let row: Option<String> =
        sqlx::query_scalar("SELECT pick_json FROM station_picks WHERE competition_id = ?")
            .bind(competition_id.to_string())
            .fetch_optional(db.pool())
            .await
            .inspect_err(|error| log::warn!("Cannot read the pick for {competition_id}: {error:#}"))
            .ok()??;
    serde_json::from_str(&row).ok()
}
