use duckdb::Connection;
use log::info;

pub fn run_users_migrations(conn: &mut Connection) -> Result<(), duckdb::Error> {
    create_version_table(conn)?;
    let mut stmt = conn.prepare("SELECT version FROM db_version")?;
    let mut rows = stmt.query([])?;

    let current_version = if let Some(row) = rows.next()? {
        row.get(0)?
    } else {
        0
    };

    match current_version {
        0 => {
            create_users_initial_schema(conn)?;
        }
        /*1 => {
        migrate_to_version_2(conn)?;
        }*/
        _ => info!("database is up-to-date."),
    }

    Ok(())
}

fn create_version_table(conn: &mut Connection) -> Result<(), duckdb::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS db_version ( version INTEGER PRIMARY KEY);",
        [],
    )?;
    Ok(())
}

pub fn create_users_initial_schema(conn: &mut Connection) -> Result<(), duckdb::Error> {
    let initial_schema = r#"
    -- TODO add password login
    CREATE TABLE IF NOT EXISTS user
    (
            nostr_pubkey TEXT NOT NULL UNIQUE,                      -- Login via verifying a random hash being signed
            encrypted_bitcoin_private_key TEXT NOT NULL UNIQUE,     -- User encrypted bitcoin key for dlctix wallet
            network                                 TEXT NOT NULL,
            created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    );
    INSERT INTO db_version (version) VALUES (1);
    "#;
    conn.execute_batch(initial_schema)?;
    Ok(())
}

/* how to add the next sql migration:
pub fn migrate_to_version_2(conn: &mut Connection) -> Result<(), duckdb::Error> {
    let migration_2 = r#"
    UPDATE db_version SET version = 2;"#;"
    conn.execute_batch(migration_2)?;
    Ok(())
}
*/
