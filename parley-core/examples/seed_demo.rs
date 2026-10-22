//! Build a demo data directory for screenshots and docs: a fresh database
//! with the fictional meetings in `demo/seed.sql`, never the user's own.
//!
//! ```sh
//! cargo run -p parley-core --example seed_demo -- /tmp/parley-demo
//! XDG_DATA_HOME=/tmp/parley-demo ./target/debug/parley
//! ```
//!
//! The argument is used as `XDG_DATA_HOME`; the database goes in
//! `<dir>/io.github.hankanman.Parley/`. An existing demo database there is
//! replaced.

use parley_core::database::manager::DatabaseManager;
use parley_core::paths::APP_IDENTIFIER;

const SEED: &str = include_str!("demo/seed.sql");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let base = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: seed_demo <dir to use as XDG_DATA_HOME>"))?;
    let data_dir = std::path::Path::new(&base).join(APP_IDENTIFIER);
    std::fs::create_dir_all(&data_dir)?;
    let db_path = data_dir.join("meeting_minutes.sqlite");
    for suffix in ["", "-wal", "-shm"] {
        let mut path = db_path.clone().into_os_string();
        path.push(suffix);
        let _ = std::fs::remove_file(path);
    }

    let db = DatabaseManager::new(&db_path.to_string_lossy()).await?;
    sqlx::raw_sql(SEED).execute(db.pool()).await?;
    db.cleanup().await?;
    println!("Demo database written to {}", db_path.display());
    Ok(())
}
