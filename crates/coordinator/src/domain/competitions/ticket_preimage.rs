//! Ticket preimages at rest.
//!
//! A ticket's preimage settles its HODL invoice and unlocks its split transaction, so the
//! database keeps it sealed in `tickets.preimage_ciphertext`: AES-256-GCM under a key derived
//! from the coordinator's private key, as `nonce || ciphertext || tag`, with the ticket's id and
//! hash as associated data so a ciphertext cannot be moved to another row.
//!
//! The key comes from the coordinator's existing private key rather than a new secret: both
//! blue/green slots already load that key, and it is already backed up, so losing a second file
//! can never lose the preimages.
//!
//! Older releases read the plaintext `tickets.encrypted_preimage` column (named before this
//! existed), so it is still written beside the ciphertext. Rows without a ciphertext, written by
//! an older release, are sealed by `backfill_ticket_preimages` and read from the plaintext
//! column until then.

use aes_gcm::{
    aead::{Aead, KeyInit, Nonce, Payload},
    Aes256Gcm,
};
use log::warn;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{CompetitionStore, Ticket};
use crate::infra::db::DatabaseWriteError;

const KEY_TAG: &[u8] = b"coordinator/ticket-preimage-key/v1";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
/// Rows the backfill seals per write.
pub const TICKET_PREIMAGE_BATCH: u32 = 100;

#[derive(Debug, thiserror::Error)]
pub enum TicketPreimageError {
    #[error("ticket {0} has no stored preimage")]
    Missing(Uuid),
    #[error("ticket {0} has a sealed preimage but this process has no ticket key")]
    NoKey(Uuid),
    #[error("ticket {0}'s sealed preimage does not open with this key, id and hash")]
    Decrypt(Uuid),
    #[error("ticket {0}'s stored preimage or hash is malformed")]
    Malformed(Uuid),
    #[error("ticket {0}'s preimage does not hash to its stored hash")]
    HashMismatch(Uuid),
    #[error("could not seal ticket {0}'s preimage")]
    Encrypt(Uuid),
}

/// Seals and opens ticket preimages. Never prints its key.
pub struct TicketCipher {
    cipher: Aes256Gcm,
}

impl std::fmt::Debug for TicketCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TicketCipher(..)")
    }
}

impl TicketCipher {
    /// The cipher keyed from the coordinator's private key: the BIP-340 style tagged hash
    /// `sha256(sha256(tag) || sha256(tag) || secret)` with tag `coordinator/ticket-preimage-key/v1`.
    /// The secret is a uniformly random 32-byte key, so one hash under its own tag separates this
    /// key from every other use of it.
    pub fn from_coordinator_key(secret: &[u8; 32]) -> Self {
        let tag = Sha256::digest(KEY_TAG);
        let key = Zeroizing::new(<[u8; 32]>::from(
            Sha256::new()
                .chain_update(tag)
                .chain_update(tag)
                .chain_update(secret)
                .finalize(),
        ));
        Self {
            cipher: Aes256Gcm::new(&(*key).into()),
        }
    }

    /// `nonce || ciphertext || tag` for `preimage`, bound to the ticket's id and hash.
    pub fn seal(
        &self,
        ticket_id: Uuid,
        hash_hex: &str,
        preimage: &[u8; 32],
    ) -> Result<Vec<u8>, TicketPreimageError> {
        let aad = associated_data(ticket_id, hash_hex)?;
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let sealed = self
            .cipher
            .encrypt(
                &Nonce::<Aes256Gcm>::from(nonce),
                Payload {
                    msg: preimage,
                    aad: &aad,
                },
            )
            .map_err(|_| TicketPreimageError::Encrypt(ticket_id))?;
        let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// The preimage `seal` sealed for this ticket id and hash.
    pub fn open(
        &self,
        ticket_id: Uuid,
        hash_hex: &str,
        sealed: &[u8],
    ) -> Result<[u8; 32], TicketPreimageError> {
        if sealed.len() != NONCE_LEN + 32 + TAG_LEN {
            return Err(TicketPreimageError::Malformed(ticket_id));
        }
        let aad = associated_data(ticket_id, hash_hex)?;
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce
            .try_into()
            .map_err(|_| TicketPreimageError::Malformed(ticket_id))?;
        let preimage = Zeroizing::new(
            self.cipher
                .decrypt(
                    &Nonce::<Aes256Gcm>::from(nonce),
                    Payload {
                        msg: ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| TicketPreimageError::Decrypt(ticket_id))?,
        );
        preimage
            .as_slice()
            .try_into()
            .map_err(|_| TicketPreimageError::Malformed(ticket_id))
    }
}

fn associated_data(ticket_id: Uuid, hash_hex: &str) -> Result<Vec<u8>, TicketPreimageError> {
    let hash = decode_hash(ticket_id, hash_hex)?;
    let mut aad = Vec::with_capacity(48);
    aad.extend_from_slice(ticket_id.as_bytes());
    aad.extend_from_slice(&hash);
    Ok(aad)
}

fn decode_hash(ticket_id: Uuid, hash_hex: &str) -> Result<[u8; 32], TicketPreimageError> {
    hex::decode(hash_hex)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(TicketPreimageError::Malformed(ticket_id))
}

/// Refuse a preimage that does not hash to the ticket's hash.
fn verified(
    ticket_id: Uuid,
    hash_hex: &str,
    preimage: [u8; 32],
) -> Result<[u8; 32], TicketPreimageError> {
    if <[u8; 32]>::from(Sha256::digest(preimage)) != decode_hash(ticket_id, hash_hex)? {
        return Err(TicketPreimageError::HashMismatch(ticket_id));
    }
    Ok(preimage)
}

/// The preimage stored for a ticket: its ciphertext when it has one, else the plaintext column
/// an older release wrote. Either way it must hash to `hash_hex`.
pub(super) fn stored_preimage(
    cipher: Option<&TicketCipher>,
    ticket_id: Uuid,
    hash_hex: &str,
    ciphertext: Option<&[u8]>,
    legacy_hex: &str,
) -> Result<[u8; 32], TicketPreimageError> {
    let preimage = match ciphertext {
        Some(sealed) => cipher
            .ok_or(TicketPreimageError::NoKey(ticket_id))?
            .open(ticket_id, hash_hex, sealed)?,
        None if legacy_hex.is_empty() => return Err(TicketPreimageError::Missing(ticket_id)),
        None => hex::decode(legacy_hex)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(TicketPreimageError::Malformed(ticket_id))?,
    };
    verified(ticket_id, hash_hex, preimage)
}

impl Ticket {
    /// The ticket's preimage; see `stored_preimage`.
    pub fn preimage(&self, cipher: Option<&TicketCipher>) -> Result<[u8; 32], TicketPreimageError> {
        stored_preimage(
            cipher,
            self.id,
            &self.hash,
            self.preimage_ciphertext.as_deref(),
            &self.legacy_preimage_hex,
        )
    }
}

impl CompetitionStore {
    /// The ticket's preimage, opened with this store's ticket key.
    pub fn ticket_preimage(&self, ticket: &Ticket) -> Result<[u8; 32], TicketPreimageError> {
        ticket.preimage(self.ticket_cipher.as_deref())
    }

    /// The ciphertext column's value for a new preimage: `None` without a ticket key.
    pub(super) fn seal_preimage(
        &self,
        ticket_id: Uuid,
        hash_hex: &str,
        preimage: &[u8; 32],
    ) -> Result<Option<Vec<u8>>, TicketPreimageError> {
        self.ticket_cipher
            .as_deref()
            .map(|cipher| cipher.seal(ticket_id, hash_hex, preimage))
            .transpose()
    }

    /// Seal the preimage of every ticket an older release stored only in plaintext, `batch`
    /// rows per write. Returns how many rows it sealed. A row whose preimage does not hash to its
    /// hash is left alone and logged; readers refuse it too.
    pub async fn backfill_ticket_preimages(&self, batch: u32) -> Result<usize, DatabaseWriteError> {
        let Some(cipher) = self.ticket_cipher.clone() else {
            return Ok(0);
        };
        let mut sealed_total = 0;
        let mut after = String::new();
        loop {
            let rows = sqlx::query(
                "SELECT id, hash, encrypted_preimage FROM tickets
                 WHERE preimage_ciphertext IS NULL AND encrypted_preimage != '' AND id > ?
                 ORDER BY id LIMIT ?",
            )
            .bind(&after)
            .bind(batch)
            .fetch_all(self.db_connection.read())
            .await?;
            let Some(last) = rows.last() else {
                return Ok(sealed_total);
            };
            after = last.try_get("id")?;
            let mut updates = Vec::with_capacity(rows.len());
            for row in &rows {
                let id: String = row.try_get("id")?;
                let hash: String = row.try_get("hash")?;
                let legacy: String = row.try_get("encrypted_preimage")?;
                let Ok(ticket_id) = Uuid::parse_str(&id) else {
                    warn!("Ticket preimage backfill skipped a ticket with id {id:?}");
                    continue;
                };
                let sealed = match stored_preimage(None, ticket_id, &hash, None, &legacy)
                    .and_then(|preimage| cipher.seal(ticket_id, &hash, &preimage))
                {
                    Ok(sealed) => sealed,
                    Err(error) => {
                        warn!("Ticket preimage backfill skipped: {error}");
                        continue;
                    }
                };
                updates.push((id, hash, legacy, sealed));
            }
            let count = self
                .db_connection
                .execute_write(move |pool| async move {
                    let mut tx = pool.begin().await?;
                    let mut count = 0;
                    for (id, hash, legacy, sealed) in &updates {
                        // A preimage rotated since the read keeps its own ciphertext.
                        count += sqlx::query(
                            "UPDATE tickets SET preimage_ciphertext = ?
                             WHERE id = ? AND hash = ? AND encrypted_preimage = ?
                               AND preimage_ciphertext IS NULL",
                        )
                        .bind(sealed)
                        .bind(id)
                        .bind(hash)
                        .bind(legacy)
                        .execute(&mut *tx)
                        .await?
                        .rows_affected() as usize;
                    }
                    tx.commit().await?;
                    Ok(count)
                })
                .await?;
            sealed_total += count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::db::{DBConnection, DatabasePoolConfig, DatabaseType};
    use std::sync::Arc;

    fn ticket_hash(preimage: &[u8; 32]) -> String {
        hex::encode(Sha256::digest(preimage))
    }

    #[test]
    fn sealed_preimage_round_trips() {
        let cipher = TicketCipher::from_coordinator_key(&[7; 32]);
        let id = Uuid::now_v7();
        let preimage = [42u8; 32];
        let hash = ticket_hash(&preimage);
        let sealed = cipher.seal(id, &hash, &preimage).unwrap();
        assert_eq!(sealed.len(), NONCE_LEN + 32 + TAG_LEN);
        assert_eq!(cipher.open(id, &hash, &sealed).unwrap(), preimage);
        // A fresh nonce each time.
        assert_ne!(cipher.seal(id, &hash, &preimage).unwrap(), sealed);
        assert_eq!(format!("{cipher:?}"), "TicketCipher(..)");
    }

    #[test]
    fn sealed_preimage_opens_only_with_its_key_and_row() {
        let cipher = TicketCipher::from_coordinator_key(&[7; 32]);
        let id = Uuid::now_v7();
        let preimage = [42u8; 32];
        let hash = ticket_hash(&preimage);
        let sealed = cipher.seal(id, &hash, &preimage).unwrap();

        let other_key = TicketCipher::from_coordinator_key(&[8; 32]);
        assert!(matches!(
            other_key.open(id, &hash, &sealed),
            Err(TicketPreimageError::Decrypt(_))
        ));
        assert!(matches!(
            cipher.open(Uuid::now_v7(), &hash, &sealed),
            Err(TicketPreimageError::Decrypt(_))
        ));
        let other_hash = ticket_hash(&[43u8; 32]);
        assert!(matches!(
            cipher.open(id, &other_hash, &sealed),
            Err(TicketPreimageError::Decrypt(_))
        ));
        let mut tampered = sealed.clone();
        tampered[NONCE_LEN] ^= 1;
        assert!(matches!(
            cipher.open(id, &hash, &tampered),
            Err(TicketPreimageError::Decrypt(_))
        ));
    }

    #[test]
    fn stored_preimage_falls_back_to_plaintext_and_checks_the_hash() {
        let cipher = TicketCipher::from_coordinator_key(&[7; 32]);
        let id = Uuid::now_v7();
        let preimage = [42u8; 32];
        let hash = ticket_hash(&preimage);
        let legacy = hex::encode(preimage);

        // Not backfilled yet: the plaintext column, with or without a key.
        assert_eq!(
            stored_preimage(Some(&cipher), id, &hash, None, &legacy).unwrap(),
            preimage
        );
        assert_eq!(
            stored_preimage(None, id, &hash, None, &legacy).unwrap(),
            preimage
        );
        // The ciphertext wins over the plaintext column.
        let sealed = cipher.seal(id, &hash, &preimage).unwrap();
        assert_eq!(
            stored_preimage(Some(&cipher), id, &hash, Some(&sealed), "").unwrap(),
            preimage
        );
        assert!(matches!(
            stored_preimage(None, id, &hash, Some(&sealed), &legacy),
            Err(TicketPreimageError::NoKey(_))
        ));
        // A preimage that does not hash to the ticket's hash is refused.
        let wrong = hex::encode([1u8; 32]);
        assert!(matches!(
            stored_preimage(Some(&cipher), id, &hash, None, &wrong),
            Err(TicketPreimageError::HashMismatch(_))
        ));
        let sealed_wrong = cipher.seal(id, &hash, &[1u8; 32]).unwrap();
        assert!(matches!(
            stored_preimage(Some(&cipher), id, &hash, Some(&sealed_wrong), ""),
            Err(TicketPreimageError::HashMismatch(_))
        ));
        assert!(matches!(
            stored_preimage(Some(&cipher), id, &hash, None, ""),
            Err(TicketPreimageError::Missing(_))
        ));
        assert!(matches!(
            stored_preimage(Some(&cipher), id, &hash, None, "preimage"),
            Err(TicketPreimageError::Malformed(_))
        ));
    }

    async fn store(directory: &tempfile::TempDir) -> (CompetitionStore, DBConnection) {
        let database = DBConnection::new(
            directory.path().to_str().unwrap(),
            "competitions",
            DatabasePoolConfig::default(),
            DatabaseType::Competitions,
        )
        .await
        .unwrap();
        let store = CompetitionStore::new(database.clone())
            .with_ticket_cipher(Arc::new(TicketCipher::from_coordinator_key(&[7; 32])));
        (store, database)
    }

    async fn insert_competition(database: &DBConnection) -> Uuid {
        let id = Uuid::now_v7();
        database
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO competitions (id, created_at, event_submission) VALUES (?, datetime('now'), '{}')",
                )
                .bind(id.to_string())
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .unwrap();
        id
    }

    async fn insert_legacy_ticket(
        database: &DBConnection,
        competition: Uuid,
        preimage_hex: &str,
        hash: &str,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let (preimage_hex, hash) = (preimage_hex.to_owned(), hash.to_owned());
        database
            .execute_write(move |pool| async move {
                sqlx::query(
                    "INSERT INTO tickets (id, event_id, encrypted_preimage, hash) VALUES (?, ?, ?, ?)",
                )
                .bind(id.to_string())
                .bind(competition.to_string())
                .bind(preimage_hex)
                .bind(hash)
                .execute(&pool)
                .await?;
                Ok(())
            })
            .await
            .unwrap();
        id
    }

    /// Run one statement with `binds` as its text parameters.
    async fn write(database: &DBConnection, statement: &'static str, binds: Vec<Bind>) {
        database
            .execute_write(move |pool| async move {
                let mut query = sqlx::query(statement);
                for bind in binds {
                    query = match bind {
                        Bind::Text(text) => query.bind(text),
                        Bind::Blob(blob) => query.bind(blob),
                    };
                }
                query.execute(&pool).await?;
                Ok(())
            })
            .await
            .unwrap();
    }

    enum Bind {
        Text(String),
        Blob(Vec<u8>),
    }

    async fn ciphertext(database: &DBConnection, id: Uuid) -> Option<Vec<u8>> {
        sqlx::query_scalar("SELECT preimage_ciphertext FROM tickets WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(database.read())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn backfill_seals_legacy_rows_once() {
        let directory = tempfile::tempdir().unwrap();
        let (store, database) = store(&directory).await;
        let competition = insert_competition(&database).await;
        let mut tickets = Vec::new();
        for byte in 0..5u8 {
            let preimage = [byte; 32];
            let id = insert_legacy_ticket(
                &database,
                competition,
                &hex::encode(preimage),
                &ticket_hash(&preimage),
            )
            .await;
            tickets.push((id, preimage));
        }
        // A dummy preimage is skipped, not fatal.
        let dummy = insert_legacy_ticket(&database, competition, "preimage", "hash").await;

        assert_eq!(store.backfill_ticket_preimages(2).await.unwrap(), 5);
        assert_eq!(store.backfill_ticket_preimages(2).await.unwrap(), 0);
        assert!(ciphertext(&database, dummy).await.is_none());
        for (id, preimage) in tickets {
            let sealed = ciphertext(&database, id).await.unwrap();
            let ticket = store.get_ticket(id).await.unwrap();
            assert_eq!(
                ticket.preimage_ciphertext.as_deref(),
                Some(sealed.as_slice())
            );
            assert_eq!(store.ticket_preimage(&ticket).unwrap(), preimage);
        }
    }

    #[tokio::test]
    async fn an_older_release_rotating_a_preimage_clears_its_stale_ciphertext() {
        let directory = tempfile::tempdir().unwrap();
        let (store, database) = store(&directory).await;
        let competition = insert_competition(&database).await;
        let preimage = [3u8; 32];
        let id = insert_legacy_ticket(
            &database,
            competition,
            &hex::encode(preimage),
            &ticket_hash(&preimage),
        )
        .await;
        store.backfill_ticket_preimages(10).await.unwrap();
        assert!(ciphertext(&database, id).await.is_some());

        // An older release rotates only the plaintext column and the hash.
        let rotated = [4u8; 32];
        write(
            &database,
            "UPDATE tickets SET encrypted_preimage = ?, hash = ? WHERE id = ?",
            vec![
                Bind::Text(hex::encode(rotated)),
                Bind::Text(ticket_hash(&rotated)),
                Bind::Text(id.to_string()),
            ],
        )
        .await;
        assert!(ciphertext(&database, id).await.is_none());
        let ticket = store.get_ticket(id).await.unwrap();
        assert_eq!(store.ticket_preimage(&ticket).unwrap(), rotated);

        store.backfill_ticket_preimages(10).await.unwrap();
        let ticket = store.get_ticket(id).await.unwrap();
        assert!(ticket.preimage_ciphertext.is_some());
        assert_eq!(store.ticket_preimage(&ticket).unwrap(), rotated);
    }

    #[tokio::test]
    async fn a_ciphertext_moved_to_another_row_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let (store, database) = store(&directory).await;
        let competition = insert_competition(&database).await;
        let (first, second) = ([5u8; 32], [6u8; 32]);
        let first_id = insert_legacy_ticket(
            &database,
            competition,
            &hex::encode(first),
            &ticket_hash(&first),
        )
        .await;
        let second_id = insert_legacy_ticket(
            &database,
            competition,
            &hex::encode(second),
            &ticket_hash(&second),
        )
        .await;
        store.backfill_ticket_preimages(10).await.unwrap();
        write(
            &database,
            "UPDATE tickets SET preimage_ciphertext = ? WHERE id = ?",
            vec![
                Bind::Blob(ciphertext(&database, first_id).await.unwrap()),
                Bind::Text(second_id.to_string()),
            ],
        )
        .await;
        let ticket = store.get_ticket(second_id).await.unwrap();
        assert!(matches!(
            store.ticket_preimage(&ticket),
            Err(TicketPreimageError::Decrypt(_))
        ));
    }
}
