use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{migrate::MigrateDatabase, Result, Sqlite, SqlitePool, Transaction};
use std::fs;
use std::path::Path;
use std::time::Duration;

#[derive(Clone)]
pub struct DatabaseManager {
    pool: SqlitePool,
}

impl DatabaseManager {
    pub async fn new(db_path: &str) -> Result<Self> {
        if let Some(parent_dir) = Path::new(db_path).parent() {
            if !parent_dir.exists() {
                fs::create_dir_all(parent_dir).map_err(|e| sqlx::Error::Io(e))?;
            }
        }

        if !Path::new(db_path).exists() {
            log::info!("Creating database at {}", db_path);
            Sqlite::create_database(db_path).await?;
        }

        // #53: tuned connect options instead of the bare `SqlitePool::connect`
        // default (a single connection, journal_mode left at whatever the
        // file already has, no busy handling). WAL + a busy timeout let
        // concurrent readers/writers from the several repositories in this
        // process coexist without "database is locked" errors; `synchronous
        // = NORMAL` is the standard safe pairing with WAL (still durable
        // across app crashes, just not against an OS-level power loss
        // mid-checkpoint, which is an acceptable tradeoff for a local
        // desktop app's own data).
        let connect_options = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(connect_options)
            .await?;

        sqlx::migrate!("./migrations").run(&pool).await?;

        Ok(DatabaseManager { pool })
    }

    /// Open (creating if needed) the database at the app's default data
    /// directory (`crate::paths::app_data_dir()` — Tauri's `app_data_dir()`
    /// resolver on Linux resolves to the exact same path, see that module's
    /// doc comment).
    pub async fn new_default() -> Result<Self> {
        // Resolve the app's data directory
        let app_data_dir = crate::paths::app_data_dir().expect("failed to get app data dir");
        if !app_data_dir.exists() {
            fs::create_dir_all(&app_data_dir).map_err(|e| sqlx::Error::Io(e))?;
        }

        let db_path = app_data_dir
            .join("meeting_minutes.sqlite")
            .to_string_lossy()
            .to_string();

        // WAL file paths for defensive cleanup
        let wal_path = app_data_dir.join("meeting_minutes.sqlite-wal");
        let shm_path = app_data_dir.join("meeting_minutes.sqlite-shm");

        log::info!("DB path: {}", db_path);

        // Try to open database with defensive WAL handling
        match Self::new(&db_path).await {
            Ok(db_manager) => {
                log::info!("Database opened successfully");
                Ok(db_manager)
            }
            Err(e) => {
                // Check if error is due to corrupted WAL file
                let error_msg = e.to_string();
                if error_msg.contains("malformed") || error_msg.contains("corrupt") {
                    log::warn!("Database appears corrupted, likely due to orphaned WAL file. Attempting recovery...");
                    log::warn!("Error details: {}", error_msg);

                    // Delete potentially corrupted WAL/SHM files
                    if wal_path.exists() {
                        match fs::remove_file(&wal_path) {
                            Ok(_) => log::info!("Removed orphaned WAL file: {:?}", wal_path),
                            Err(e) => log::warn!("Failed to remove WAL file: {}", e),
                        }
                    }
                    if shm_path.exists() {
                        match fs::remove_file(&shm_path) {
                            Ok(_) => log::info!("Removed orphaned SHM file: {:?}", shm_path),
                            Err(e) => log::warn!("Failed to remove SHM file: {}", e),
                        }
                    }

                    // Retry connection without WAL files
                    log::info!("Retrying database connection after WAL cleanup...");
                    match Self::new(&db_path).await {
                        Ok(db_manager) => {
                            log::info!("Database opened successfully after WAL recovery");
                            Ok(db_manager)
                        }
                        Err(retry_err) => {
                            log::error!(
                                "Database connection failed even after WAL cleanup: {}",
                                retry_err
                            );
                            Err(retry_err)
                        }
                    }
                } else {
                    // Not a WAL-related error, propagate original error
                    log::error!("Database connection failed: {}", error_msg);
                    Err(e)
                }
            }
        }
    }

    /// Check if this is the first launch (sqlite database doesn't exist yet)
    pub async fn is_first_launch() -> Result<bool> {
        let app_data_dir = crate::paths::app_data_dir().expect("failed to get app data dir");

        let db_path = app_data_dir.join("meeting_minutes.sqlite");

        Ok(!db_path.exists())
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn with_transaction<T, F, Fut>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Transaction<'_, Sqlite>) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut tx = self.pool.begin().await?;
        let result = f(&mut tx).await;

        match result {
            Ok(val) => {
                tx.commit().await?;
                Ok(val)
            }
            Err(err) => {
                tx.rollback().await?;
                Err(err)
            }
        }
    }

    /// Cleanup database connection and checkpoint WAL
    /// This should be called on application shutdown to ensure:
    /// - All WAL changes are written to the main database file
    /// - The .wal and .shm files are deleted
    /// - Connection pool is gracefully closed
    pub async fn cleanup(&self) -> Result<()> {
        log::info!("Starting database cleanup...");

        // Force checkpoint of WAL to main database file and remove WAL file
        // TRUNCATE mode: checkpoints all pages AND deletes the WAL file
        match sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&self.pool)
            .await
        {
            Ok(_) => log::info!("WAL checkpoint completed successfully"),
            Err(e) => log::warn!("WAL checkpoint failed (non-fatal): {}", e),
        }

        // Close the connection pool gracefully
        self.pool.close().await;
        log::info!("Database connection pool closed");

        Ok(())
    }
}
