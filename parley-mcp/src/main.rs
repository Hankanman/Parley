//! `parley-mcp` — an MCP stdio server over the Parley meeting database.
//!
//! Spawned by an MCP client (Claude Desktop / Claude Code), it speaks JSON-RPC
//! over stdin/stdout and exposes the user's meetings, transcripts, summaries,
//! action items, and notes as MCP tools and resources. It talks to SQLite
//! directly, so it works whether or not the Parley app is running.
//!
//! See README.md for client configuration.

mod cli;
mod db;
mod server;

use std::process::ExitCode;

use rmcp::transport::stdio;
use rmcp::ServiceExt;

use crate::cli::{Action, HELP};
use crate::server::ParleyMcp;

#[tokio::main]
async fn main() -> ExitCode {
    // stdout is the MCP protocol channel — anything written there that isn't
    // JSON-RPC corrupts the session. All diagnostics go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "parley_mcp=info,rmcp=warn".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("parley-mcp: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let args = match cli::parse_args(std::env::args().skip(1))? {
        Action::PrintHelp => {
            println!("{HELP}");
            return Ok(());
        }
        Action::PrintVersion => {
            println!("parley-mcp {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Action::Serve(args) => args,
    };

    let db_path = cli::resolve_db_path(
        args.db,
        cli::db_path_from_env(),
        cli::default_db_path(),
    )?;
    cli::check_db_exists(&db_path)?;

    tracing::info!(path = %db_path.display(), "opening Parley database");
    let pool = db::connect(&db_path)
        .await
        .map_err(|e| format!("failed to open {}: {e}", db_path.display()))?;

    // Fail loudly at startup rather than on the agent's first tool call: a DB
    // from an older app version won't have these tables, and "no such table"
    // surfacing mid-conversation is much harder to act on.
    verify_schema(&pool).await?;

    let service = ParleyMcp::new(pool)
        .serve(stdio())
        .await
        .map_err(|e| format!("failed to start MCP server: {e}"))?;

    tracing::info!("parley-mcp ready on stdio");

    // Resolves when the client disconnects (closes stdin) or cancels.
    let reason = service
        .waiting()
        .await
        .map_err(|e| format!("MCP server stopped with an error: {e}"))?;
    tracing::info!(?reason, "parley-mcp shutting down");

    Ok(())
}

/// Tables this server needs, each with one column that pins the *shape* we
/// expect — not just the name.
///
/// Checking a column matters as much as checking the table: a table can exist
/// under the right name with the wrong schema if an older migration created it
/// first (`CREATE TABLE IF NOT EXISTS` silently no-ops in that case). Then
/// every read looks fine and the first write dies with "no such column" in the
/// middle of the agent's conversation, which is a miserable thing to debug from
/// the client side.
const REQUIRED_SCHEMA: [(&str, &str); 5] = [
    ("meetings", "title"),
    ("transcripts", "transcript"),
    ("summary_processes", "result"),
    ("action_items", "status"),
    ("meeting_notes", "body"),
];

/// Check the tables and columns we depend on exist, naming what's missing.
async fn verify_schema(pool: &sqlx::SqlitePool) -> Result<(), String> {
    for (table, column) in REQUIRED_SCHEMA {
        let table_exists: Option<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?")
                .bind(table)
                .fetch_optional(pool)
                .await
                .map_err(|e| format!("failed to inspect database schema: {e}"))?;

        if table_exists.is_none() {
            return Err(migration_hint(&format!(
                "the database is missing the `{table}` table"
            )));
        }

        let column_exists: Option<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info(?) WHERE name = ?")
                .bind(table)
                .bind(column)
                .fetch_optional(pool)
                .await
                .map_err(|e| format!("failed to inspect `{table}` columns: {e}"))?;

        if column_exists.is_none() {
            return Err(migration_hint(&format!(
                "the `{table}` table exists but has no `{column}` column — it \
                 isn't the schema this server expects"
            )));
        }
    }
    Ok(())
}

fn migration_hint(problem: &str) -> String {
    format!(
        "{problem}, so this server can't run against it.\n\nThis usually means \
         the Parley app hasn't run its migrations yet, or is older than this \
         server. Open Parley once to migrate the database, then retry."
    )
}
