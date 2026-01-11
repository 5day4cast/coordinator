use crate::SqliteConfigSerde;
use log::debug;
use sqlx::{
    migrate::MigrateDatabase,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow},
    Row, Sqlite, SqlitePool,
};
use std::{
    env,
    hash::{DefaultHasher, Hash, Hasher},
    time::Duration as StdDuration,
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Debug, Clone)]
pub enum SqliteMode {
    ReadWriteCreate, // rwc - Read + Write + Create if missing
    ReadWrite,       // rw  - Read + Write (must exist)
    ReadOnly,        // ro  - Read only (must exist)
    Memory,          // memory - In-memory database
}

#[derive(Debug, Clone)]
pub enum SqliteCache {
    Shared,
    Private,
}

#[derive(Debug, Clone)]
pub enum JournalMode {
    WAL,      // Best for concurrent access
    DELETE,   // Default, good for single writer
    TRUNCATE, // Faster than DELETE
    PERSIST,  // Keeps journal file
    MEMORY,   // In-memory journal
    OFF,      // No journal (dangerous)
}

#[derive(Debug, Clone)]
pub enum SynchronousMode {
    OFF,    // Fastest, least safe
    NORMAL, // Good balance
    FULL,   // Safest, slower
    EXTRA,  // Paranoid mode
}

#[derive(Debug, Clone)]
pub enum TempStore {
    Default, // Use compile-time default
    File,    // Temporary tables in files
    Memory,  // Temporary tables in memory
}

#[derive(Debug, Clone)]
pub struct SqliteConfig {
    pub mode: SqliteMode,
    pub cache: SqliteCache,
    pub busy_timeout_ms: u32,
    pub journal_mode: JournalMode,
    pub synchronous: SynchronousMode,
    pub cache_size: i32,
    pub foreign_keys: bool,
    pub wal_autocheckpoint: Option<u32>,
    pub temp_store: TempStore,
    pub mmap_size: Option<u64>,
    pub page_size: Option<u32>,
}

impl SqliteConfig {
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(timeout) = env::var("SQLITE_BUSY_TIMEOUT_MS") {
            config.busy_timeout_ms = timeout.parse().unwrap_or(config.busy_timeout_ms);
        }

        if let Ok(cache_size) = env::var("SQLITE_CACHE_SIZE") {
            config.cache_size = cache_size.parse().unwrap_or(config.cache_size);
        }

        if let Ok(journal_mode) = env::var("SQLITE_JOURNAL_MODE") {
            config.journal_mode = match journal_mode.to_uppercase().as_str() {
                "WAL" => JournalMode::WAL,
                "DELETE" => JournalMode::DELETE,
                "TRUNCATE" => JournalMode::TRUNCATE,
                "PERSIST" => JournalMode::PERSIST,
                "MEMORY" => JournalMode::MEMORY,
                "OFF" => JournalMode::OFF,
                _ => config.journal_mode,
            };
        }

        if let Ok(sync_mode) = env::var("SQLITE_SYNCHRONOUS") {
            config.synchronous = match sync_mode.to_uppercase().as_str() {
                "OFF" => SynchronousMode::OFF,
                "NORMAL" => SynchronousMode::NORMAL,
                "FULL" => SynchronousMode::FULL,
                "EXTRA" => SynchronousMode::EXTRA,
                _ => config.synchronous,
            };
        }

        if let Ok(foreign_keys) = env::var("SQLITE_FOREIGN_KEYS") {
            config.foreign_keys = foreign_keys.parse().unwrap_or(config.foreign_keys);
        }

        config
    }

    pub fn development() -> Self {
        Self {
            busy_timeout_ms: 10000, // More patience in dev
            cache_size: 100000,     // Smaller cache for dev
            ..Default::default()
        }
    }

    pub fn production() -> Self {
        Self {
            synchronous: SynchronousMode::FULL, // Maximum safety
            cache_size: 2000000,                // 2GB cache
            wal_autocheckpoint: Some(10000),    // Less frequent checkpoints
            mmap_size: Some(1073741824),        // 1GB memory-mapped I/O
            ..Default::default()
        }
    }

    pub fn read_only() -> Self {
        Self {
            mode: SqliteMode::ReadOnly,
            cache: SqliteCache::Shared,
            journal_mode: JournalMode::WAL, // Can still read WAL files
            synchronous: SynchronousMode::OFF, // No writes, so sync doesn't matter
            foreign_keys: false,            // Read-only doesn't need FK checks
            ..Default::default()
        }
    }

    pub fn testing() -> Self {
        Self {
            mode: SqliteMode::Memory,
            cache: SqliteCache::Shared,
            journal_mode: JournalMode::MEMORY,
            synchronous: SynchronousMode::OFF,
            temp_store: TempStore::Memory,
            mmap_size: None,
            ..Default::default()
        }
    }

    pub fn build_connect_options(&self, database_path: &str) -> SqliteConnectOptions {
        let mut options = SqliteConnectOptions::new();

        match self.mode {
            SqliteMode::Memory => {
                // For memory mode, use a shared memory database so read/write pools
                // can access the same in-memory tables
                // Create deterministic unique name from the database_path,
                // We use a hash to keep from running into path character issues
                // This ensures read/write pools in same test use same memory DB
                // Format: "file:memdb1?mode=memory&cache=shared"
                let mut hasher = DefaultHasher::new();
                database_path.hash(&mut hasher);
                let unique_id = hasher.finish();

                let memory_uri = format!("file:memdb_{}?mode=memory&cache=shared", unique_id);
                options = options.filename(&memory_uri);
            }
            SqliteMode::ReadOnly => {
                return options
                    .filename(database_path)
                    .read_only(true)
                    .shared_cache(matches!(self.cache, SqliteCache::Shared))
                    .busy_timeout(StdDuration::from_millis(self.busy_timeout_ms as u64));
            }
            SqliteMode::ReadWrite => {
                options = options.filename(database_path).read_only(false);
            }
            SqliteMode::ReadWriteCreate => {
                options = options
                    .filename(database_path)
                    .read_only(false)
                    .create_if_missing(true);
            }
        }

        // Cache mode - for memory databases, we always want shared cache
        match self.mode {
            SqliteMode::Memory => {
                // Memory databases should always use shared cache
                options = options.shared_cache(true);
            }
            _ => {
                // For file databases, use the configured cache mode
                match self.cache {
                    SqliteCache::Shared => {
                        options = options.shared_cache(true);
                    }
                    SqliteCache::Private => {
                        options = options.shared_cache(false);
                    }
                }
            }
        }

        // Rest of the configuration stays the same...
        options = options.busy_timeout(StdDuration::from_millis(self.busy_timeout_ms as u64));

        let journal_mode = match self.journal_mode {
            JournalMode::WAL => "WAL",
            JournalMode::DELETE => "DELETE",
            JournalMode::TRUNCATE => "TRUNCATE",
            JournalMode::PERSIST => "PERSIST",
            JournalMode::MEMORY => "MEMORY",
            JournalMode::OFF => "OFF",
        };
        options = options.pragma("journal_mode", journal_mode);

        let sync_mode = match self.synchronous {
            SynchronousMode::OFF => "OFF",
            SynchronousMode::NORMAL => "NORMAL",
            SynchronousMode::FULL => "FULL",
            SynchronousMode::EXTRA => "EXTRA",
        };
        options = options.pragma("synchronous", sync_mode);

        options = options.pragma("cache_size", self.cache_size.to_string());

        if self.foreign_keys {
            options = options.pragma("foreign_keys", "true");
        }

        if let Some(checkpoint) = self.wal_autocheckpoint {
            options = options.pragma("wal_autocheckpoint", checkpoint.to_string());
        }

        let temp_store = match self.temp_store {
            TempStore::Default => "default",
            TempStore::File => "file",
            TempStore::Memory => "memory",
        };
        options = options.pragma("temp_store", temp_store);

        if let Some(mmap_size) = self.mmap_size {
            options = options.pragma("mmap_size", mmap_size.to_string());
        }

        if let Some(page_size) = self.page_size {
            options = options.pragma("page_size", page_size.to_string());
        }

        options
    }
}

impl Default for SqliteConfig {
    fn default() -> Self {
        Self {
            mode: SqliteMode::ReadWriteCreate,
            cache: SqliteCache::Shared,
            busy_timeout_ms: 5000,
            journal_mode: JournalMode::WAL,
            synchronous: SynchronousMode::NORMAL,
            cache_size: 1000000,
            foreign_keys: true,
            wal_autocheckpoint: Some(1000),
            temp_store: TempStore::Memory,
            mmap_size: Some(268435456), // 256MB
            page_size: Some(4096),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DatabasePoolConfig {
    pub read_max_connections: u32,
    pub read_min_connections: u32,
    pub write_max_connections: u32,
    pub write_min_connections: u32,
    pub idle_timeout_secs: u64,
    pub acquire_timeout_secs: u64,
    pub sqlite_config: SqliteConfig,
}

impl Default for DatabasePoolConfig {
    fn default() -> Self {
        Self {
            read_max_connections: 12, // More readers
            read_min_connections: 2,
            write_max_connections: 5, // Fewer writers
            write_min_connections: 1,
            idle_timeout_secs: 600,   // 10 minutes
            acquire_timeout_secs: 15, // 15 seconds
            sqlite_config: SqliteConfig::default(),
        }
    }
}

impl DatabasePoolConfig {
    pub fn development() -> Self {
        Self {
            read_max_connections: 5,
            write_max_connections: 3,
            sqlite_config: SqliteConfig::development(),
            ..Default::default()
        }
    }

    pub fn production() -> Self {
        Self {
            read_max_connections: 20,
            read_min_connections: 5,
            write_max_connections: 8,
            write_min_connections: 2,
            acquire_timeout_secs: 30,
            sqlite_config: SqliteConfig::production(),
            ..Default::default()
        }
    }

    pub fn testing() -> Self {
        Self {
            read_max_connections: 2,
            write_max_connections: 1,
            acquire_timeout_secs: 5,
            sqlite_config: SqliteConfig::testing(),
            ..Default::default()
        }
    }
}

impl From<SqliteConfigSerde> for SqliteConfig {
    fn from(config: SqliteConfigSerde) -> Self {
        Self {
            mode: match config.mode.as_str() {
                "ReadWriteCreate" => SqliteMode::ReadWriteCreate,
                "ReadWrite" => SqliteMode::ReadWrite,
                "ReadOnly" => SqliteMode::ReadOnly,
                "Memory" => SqliteMode::Memory,
                _ => SqliteMode::ReadWriteCreate,
            },
            cache: match config.cache.as_str() {
                "Shared" => SqliteCache::Shared,
                "Private" => SqliteCache::Private,
                _ => SqliteCache::Shared,
            },
            busy_timeout_ms: config.busy_timeout_ms,
            journal_mode: match config.journal_mode.as_str() {
                "WAL" => JournalMode::WAL,
                "DELETE" => JournalMode::DELETE,
                "TRUNCATE" => JournalMode::TRUNCATE,
                "PERSIST" => JournalMode::PERSIST,
                "MEMORY" => JournalMode::MEMORY,
                "OFF" => JournalMode::OFF,
                _ => JournalMode::WAL,
            },
            synchronous: match config.synchronous.as_str() {
                "OFF" => SynchronousMode::OFF,
                "NORMAL" => SynchronousMode::NORMAL,
                "FULL" => SynchronousMode::FULL,
                "EXTRA" => SynchronousMode::EXTRA,
                _ => SynchronousMode::NORMAL,
            },
            cache_size: config.cache_size,
            foreign_keys: config.foreign_keys,
            wal_autocheckpoint: config.wal_autocheckpoint,
            temp_store: match config.temp_store.as_str() {
                "Default" => TempStore::Default,
                "File" => TempStore::File,
                "Memory" => TempStore::Memory,
                _ => TempStore::Memory,
            },
            mmap_size: config.mmap_size,
            page_size: config.page_size,
        }
    }
}

impl From<crate::config::DBSettings> for DatabasePoolConfig {
    fn from(config: crate::config::DBSettings) -> Self {
        Self {
            read_max_connections: config.read_max_connections,
            read_min_connections: config.read_min_connections,
            write_max_connections: config.write_max_connections,
            write_min_connections: config.write_min_connections,
            idle_timeout_secs: config.idle_timeout_secs,
            acquire_timeout_secs: config.acquire_timeout_secs,
            sqlite_config: config.sqlite_config.into(),
        }
    }
}

static COMPETITIONS_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/competitions");
static USERS_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/users");

pub enum DatabaseType {
    Competitions,
    Users,
}

#[derive(Clone, Debug)]
pub struct DBConnection {
    pub database_name: String,
    pub database_path: String,
    read_pool: SqlitePool,
    write_pool: SqlitePool,
}

impl DBConnection {
    pub async fn new(
        path: &str,
        db_name: &str,
        database_pool_config: DatabasePoolConfig,
        db_type: DatabaseType,
    ) -> Result<Self, sqlx::Error> {
        let database_path = format!("{}/{}.db", path, db_name);

        // Create database file if it doesn't exist (unless it's memory mode)
        if !matches!(database_pool_config.sqlite_config.mode, SqliteMode::Memory) {
            let database_url = format!("sqlite:{}", database_path);
            if !Sqlite::database_exists(&database_url).await? {
                Sqlite::create_database(&database_url).await?;
            }
        }

        // Create separate read and write pools
        let (read_pool, write_pool) =
            Self::create_pools(&database_path, &database_pool_config).await?;

        let migrator = match db_type {
            DatabaseType::Competitions => &COMPETITIONS_MIGRATOR,
            DatabaseType::Users => &USERS_MIGRATOR,
        };

        migrator
            .run(&write_pool)
            .await
            .map_err(|e| sqlx::Error::Migrate(Box::new(e)))?;

        Ok(Self {
            database_name: db_name.to_string(),
            database_path: database_path.clone(),
            read_pool,
            write_pool,
        })
    }

    #[cfg(test)]
    pub fn new_with_pools(
        database_name: String,
        database_path: String,
        read_pool: SqlitePool,
        write_pool: SqlitePool,
    ) -> Self {
        Self {
            database_name,
            database_path,
            read_pool,
            write_pool,
        }
    }

    async fn create_pools(
        database_path: &str,
        database_pool_config: &DatabasePoolConfig,
    ) -> Result<(SqlitePool, SqlitePool), sqlx::Error> {
        let (read_config, write_config) =
            if matches!(database_pool_config.sqlite_config.mode, SqliteMode::Memory) {
                // For memory mode, both pools use memory but can be separate instances
                let read_config = SqliteConfig {
                    mode: SqliteMode::Memory,
                    cache_size: database_pool_config.sqlite_config.cache_size * 2, // Larger cache for reads
                    synchronous: SynchronousMode::OFF, // No writes = no sync needed
                    ..database_pool_config.sqlite_config.clone()
                };

                let write_config = SqliteConfig {
                    mode: SqliteMode::Memory,
                    ..database_pool_config.sqlite_config.clone()
                };

                (read_config, write_config)
            } else {
                // For file mode, create separate read-only and read-write pools
                let mut read_config = SqliteConfig::read_only();
                read_config.cache_size = database_pool_config.sqlite_config.cache_size * 2; // Larger cache for reads
                read_config.busy_timeout_ms = database_pool_config.sqlite_config.busy_timeout_ms;

                let write_config = SqliteConfig {
                    mode: SqliteMode::ReadWrite,
                    ..database_pool_config.sqlite_config.clone()
                };

                (read_config, write_config)
            };

        let write_connection = write_config.build_connect_options(database_path);
        debug!("Write connection: {:?}", write_connection);

        let write_pool = SqlitePoolOptions::new()
            .max_connections(database_pool_config.write_max_connections)
            .min_connections(database_pool_config.write_min_connections)
            .acquire_timeout(StdDuration::from_secs(
                database_pool_config.acquire_timeout_secs,
            ))
            .idle_timeout(StdDuration::from_secs(
                database_pool_config.idle_timeout_secs,
            ))
            .connect_with(write_connection)
            .await?;

        let read_connection = read_config.build_connect_options(database_path);
        debug!("Read connection: {:?}", read_connection);

        let read_pool = SqlitePoolOptions::new()
            .max_connections(database_pool_config.read_max_connections)
            .min_connections(database_pool_config.read_min_connections)
            .acquire_timeout(StdDuration::from_secs(
                database_pool_config.acquire_timeout_secs,
            ))
            .idle_timeout(StdDuration::from_secs(
                database_pool_config.idle_timeout_secs,
            ))
            .connect_with(read_connection)
            .await?;

        Ok((read_pool, write_pool))
    }

    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        let _: i32 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&self.read_pool)
            .await?;
        let _: i32 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&self.write_pool)
            .await?;

        Ok(())
    }

    pub async fn close(self) {
        self.read_pool.close().await;
        self.write_pool.close().await;
    }

    pub fn read(&self) -> &SqlitePool {
        &self.read_pool
    }

    pub fn write(&self) -> &SqlitePool {
        &self.write_pool
    }
}

pub fn parse_required_datetime(
    row: &SqliteRow,
    column: &str,
) -> Result<OffsetDateTime, sqlx::Error> {
    let date_str: String = row.get(column);
    OffsetDateTime::parse(&date_str, &Rfc3339).map_err(|e| sqlx::Error::ColumnDecode {
        index: column.to_string(),
        source: Box::new(e),
    })
}

pub fn parse_optional_datetime(
    row: &SqliteRow,
    column: &str,
) -> Result<Option<OffsetDateTime>, sqlx::Error> {
    row.get::<Option<String>, _>(column)
        .map(|s| OffsetDateTime::parse(&s, &Rfc3339))
        .transpose()
        .map_err(|e| sqlx::Error::ColumnDecode {
            index: column.to_string(),
            source: Box::new(e),
        })
}

pub fn parse_optional_sqlite_datetime(
    row: &SqliteRow,
    column: &str,
) -> Result<Option<OffsetDateTime>, sqlx::Error> {
    row.get::<Option<String>, _>(column)
        .map(|s| {
            // SQLite datetime format: "YYYY-MM-DD HH:MM:SS"
            // Parse as UTC since SQLite doesn't include timezone info
            let format =
                time::format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second]")
                    .map_err(|e| sqlx::Error::ColumnDecode {
                        index: column.to_string(),
                        source: Box::new(e),
                    })?;

            let pdt = time::PrimitiveDateTime::parse(&s, &format).map_err(|e| {
                sqlx::Error::ColumnDecode {
                    index: column.to_string(),
                    source: Box::new(e),
                }
            })?;

            Ok(pdt.assume_utc())
        })
        .transpose()
}

pub fn parse_optional_blob_json<T>(row: &SqliteRow, column: &str) -> Result<Option<T>, sqlx::Error>
where
    T: serde::de::DeserializeOwned,
{
    let bytes: Option<Vec<u8>> = row.get(column);
    match bytes {
        Some(data) => {
            let parsed = serde_json::from_slice(&data).map_err(|e| sqlx::Error::ColumnDecode {
                index: column.to_string(),
                source: Box::new(e),
            })?;
            Ok(Some(parsed))
        }
        None => Ok(None),
    }
}

pub fn parse_required_blob_json<T>(row: &SqliteRow, column: &str) -> Result<T, sqlx::Error>
where
    T: serde::de::DeserializeOwned,
{
    let bytes: Vec<u8> = row.get(column);
    serde_json::from_slice(&bytes).map_err(|e| sqlx::Error::ColumnDecode {
        index: column.to_string(),
        source: Box::new(e),
    })
}
