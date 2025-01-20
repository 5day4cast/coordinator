use duckdb::Connection;
use log::info;

pub fn run_comeptition_migrations(conn: &mut Connection) -> Result<(), duckdb::Error> {
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
            create_comeptitions_initial_schema(conn)?;
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

pub fn create_comeptitions_initial_schema(conn: &mut Connection) -> Result<(), duckdb::Error> {
    let initial_schema = r#"
    -- Table of information about the oracle, mostly to prevent multiple keys from being used with the same database
    -- singleton_constant is a dummy column to ensure there is only one row
    CREATE TABLE IF NOT EXISTS coordinator_metadata
    (
            pubkey             BLOB     NOT NULL UNIQUE PRIMARY KEY, -- pubkey to private key coordinator will use to sign
            name               TEXT      NOT NULL UNIQUE,
            created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            singleton_constant BOOLEAN   NOT NULL DEFAULT TRUE, -- make sure there is only one row
            CONSTRAINT one_row_check UNIQUE (singleton_constant)
    );

    CREATE TABLE IF NOT EXISTS competitions
    (
        id UUID PRIMARY KEY,
        created_at TIMESTAMPTZ NOT NULL,
        total_competition_pool BIGINT NOT NULL,
        total_allowed_entries BIGINT NOT NULL,
        entry_fee BIGINT NOT NULL,
        event_annoucement BLOB NOT NULL,
        funding_transaction BLOB,                -- Funding transaction outpoint
        contract_parameters BLOB,                -- DLC contract parameters
        public_nonces BLOB,                      -- Coordinator's public nonces
        aggregated_nonces BLOB,                  -- Aggregated nonces from all participants
        partial_signatures BLOB,                 -- Coordinator's partial signatures
        signed_contract BLOB,                    -- Final signed contract
        contracted_at TIMESTAMPTZ,              -- When contract parameters were created
        signed_at TIMESTAMPTZ,                  -- When musig2 signing completed
        funding_broadcasted_at TIMESTAMPTZ,     -- When funding transaction was broadcast
        cancelled_at TIMESTAMPTZ,               -- If competition was cancelled
        failed_at TIMESTAMPTZ,                  -- If competition failed
        errors BLOB                             -- List of errors that lead to failed_at
    );

    CREATE TABLE IF NOT EXISTS entries
    (
        id UUID PRIMARY KEY,
        event_id UUID NOT NULL REFERENCES competitions (id),
        pubkey STRING NOT NULL,                 -- user nostr pubkey
        ephemeral_pubkey TEXT NOT NULL,         -- user ephemeral pubkey for DLC
        ephemeral_privatekey_user_encrypted TEXT NOT NULL,  -- store for better UX, backed up in user wallet
        ephemeral_privatekey TEXT,              -- provided by user on payout, encrypted by coordinator_key
        public_nonces BLOB,                     -- player signed nonces during musig signing session
        partial_signatures BLOB,                -- player partial signatures
        ticket_preimage TEXT,                   -- market maker generated preimage user needs for winnings
        ticket_hash TEXT,                       -- hash of market marker preimage
        payout_preimage_user_encrypted TEXT NOT NULL, -- store for better UX, backed up in user wallet
        payout_hash TEXT NOT NULL,              -- user provided hash of preimage to get winnings
        payout_preimage TEXT,                   -- provided by user on payout, encrypted by coordinator_key
        signed_at TIMESTAMPTZ,                  -- when user completes musig signing
        paid_at TIMESTAMPTZ                     -- when user pays for the ticket
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
