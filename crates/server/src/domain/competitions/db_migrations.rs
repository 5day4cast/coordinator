use duckdb::Connection;
use log::info;

pub fn run_migrations(conn: &mut Connection) -> Result<(), duckdb::Error> {
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
            create_initial_schema(conn)?;
        }
        /*1 => {
        migrate_to_version_2(conn)?;
        }*/
        _ => info!("database is up-to-date."),
    }

    Ok(())
}

pub fn create_version_table(conn: &mut Connection) -> Result<(), duckdb::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS db_version ( version INTEGER PRIMARY KEY);",
        [],
    )?;
    Ok(())
}

pub fn create_initial_schema(conn: &mut Connection) -> Result<(), duckdb::Error> {
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
        id UUID PRIMARY KEY, -- should match event_id with oracle
        created_at TIMESTAMPTZ NOT NULL,
        total_competition_pool BIGINT NOT NULL,
        total_allowed_entries BIGINT NOT NULL,
        entry_fee BIGINT NOT NULL,
        event_annoucement BLOB NOT NULL,
        funding_transaction BLOB,
        contract_parameters BLOB,
        public_nonces BLOB,
        cancelled_at TIMESTAMPTZ,
        -- if not enough users have entered before contract created, cancel the competition (make this 1 hour before observation time begins to start)
        -- need to send to the oracle to cancel the event
        contracted_at TIMESTAMPTZ, -- when the contract parameters have been created
        signed_at TIMESTAMPTZ,  -- when the musig signing session completes
        funding_broadcasted_at TIMESTAMPTZ, -- when funding transaction broadcasted
        failed_at TIMESTAMPTZ,
        errors BLOB -- list of errors that lead to failed_at being added
    );

    CREATE TABLE IF NOT EXISTS entries
    (
        id UUID PRIMARY KEY, -- should match entry_id with the oracle
        event_id UUID NOT NULL REFERENCES competitions (id), -- should match event_id with the oracle
        pubkey STRING NOT NULL, -- user wallet pubkey, used to authenticate/authorize messages
        -- user ephemeral pubkey, secret may be used by market maker to recover entries split TX output unilaterally.
        -- May be used more than once in a single competition/event by the user, but not across multiple competitions/events
        ephemeral_pubkey TEXT NOT NULL,
        ephemeral_privatekey_user_encrypted TEXT NOT NULL,  -- store here for better user experience, backed up in nostr relays
        ephemeral_privatekey TEXT, -- provided by the user on payout depending on method, encrypted by coordinator_key
        signed_nonces BLOB, -- player signed nonces during musig signing session, needs to be restarted if player never signs
        ticket_preimage TEXT NOT NULL, -- market maker generated preimage user needs to get winnings, encrypted by coordinator_key
        ticket_hash TEXT NOT NULL, -- hash of market marker preimage
        payout_preimage_user_encrypted TEXT NOT NULL, -- store here for better user experience, backed up in nostr relays
        payout_hash TEXT NOT NULL, -- user provided ephemeral hash of preimage to get winnings
        payout_preimage TEXT, -- provided by the user on payout,  encrypted by coordinator_key
        signed_at TIMESTAMPTZ, -- when user signs in musig
        paid_at TIMESTAMPTZ -- when user pays for the ticket
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
