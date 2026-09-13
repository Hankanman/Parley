//! Command-line argument parsing and database-path resolution.
//!
//! Hand-rolled rather than clap: the surface is three flags and keeping the
//! dependency out shaves a meaningful chunk off the binary (this crate builds
//! with `opt-level = "s"`; see the workspace root `Cargo.toml`).

use std::path::{Path, PathBuf};

pub const APP_IDENTIFIER: &str = "io.github.hankanman.Parley";
/// Data-directory name before the Parley rename. The app moves it to
/// [`APP_IDENTIFIER`] on startup; until it has, the database is still here.
pub const LEGACY_APP_IDENTIFIER: &str = "com.meetily.ai";
pub const DB_FILENAME: &str = "meeting_minutes.sqlite";
pub const DB_PATH_ENV: &str = "PARLEY_DB_PATH";
/// Pre-rename name of [`DB_PATH_ENV`], still honoured so existing MCP client
/// configs keep working.
pub const LEGACY_DB_PATH_ENV: &str = "MEETILY_DB_PATH";

/// Parsed command line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// Explicit `--db <path>` override, if given.
    pub db: Option<PathBuf>,
    pub help: bool,
    pub version: bool,
}

/// What the caller should do after parsing.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Serve(Args),
    PrintHelp,
    PrintVersion,
}

pub const HELP: &str = concat!(
    "parley-mcp ",
    env!("CARGO_PKG_VERSION"),
    "

An MCP (Model Context Protocol) server exposing the Parley meeting database
(meetings, transcripts, summaries, action items, notes) to an AI agent.

It speaks JSON-RPC over stdin/stdout — it is not run by hand, it is spawned by
an MCP client (Claude Desktop, Claude Code). See the crate README for the
client config snippet.

USAGE:
    parley-mcp [--db <path>]

OPTIONS:
    --db <path>    Path to the Parley SQLite database. Overrides $",
    "PARLEY_DB_PATH",
    ".
    -h, --help     Print this help.
    -V, --version  Print version.

DATABASE PATH RESOLUTION (first match wins):
    1. --db <path>
    2. $PARLEY_DB_PATH
    3. <data-dir>/io.github.hankanman.Parley/meeting_minutes.sqlite
       (on Linux: ~/.local/share/io.github.hankanman.Parley/meeting_minutes.sqlite)

The database must already exist — this server never creates or migrates it;
the Parley app owns the schema. The server opens the DB in WAL mode with a
busy timeout so it can read and write safely while Parley is running.

LOGGING:
    Logs go to stderr (stdout is the MCP protocol channel). Set RUST_LOG,
    e.g. RUST_LOG=parley_mcp=debug.
"
);

/// Parse an argument list (excluding argv\[0\]).
pub fn parse_args<I, S>(args: I) -> Result<Action, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut parsed = Args::default();
    let mut iter = args.into_iter().peekable();

    while let Some(arg) = iter.next() {
        match arg.as_ref() {
            "-h" | "--help" => return Ok(Action::PrintHelp),
            "-V" | "--version" => return Ok(Action::PrintVersion),
            "--db" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--db requires a path argument".to_string())?;
                parsed.db = Some(PathBuf::from(value.as_ref()));
            }
            other if other.starts_with("--db=") => {
                parsed.db = Some(PathBuf::from(&other["--db=".len()..]));
            }
            other => {
                return Err(format!(
                    "unrecognized argument: {other}\n\nRun `parley-mcp --help` for usage."
                ));
            }
        }
    }

    Ok(Action::Serve(parsed))
}

/// The default database location: `<data-dir>/io.github.hankanman.Parley/meeting_minutes.sqlite`,
/// or the pre-rename `com.meetily.ai` one if the app hasn't migrated it yet.
pub fn default_db_path() -> Option<PathBuf> {
    dirs::data_dir().map(|base| default_db_path_in(&base))
}

/// [`default_db_path`] against an explicit data dir, for tests.
pub fn default_db_path_in(base: &Path) -> PathBuf {
    let current = base.join(APP_IDENTIFIER);
    let legacy = base.join(LEGACY_APP_IDENTIFIER);
    if !current.exists() && legacy.is_dir() {
        legacy.join(DB_FILENAME)
    } else {
        current.join(DB_FILENAME)
    }
}

/// `$PARLEY_DB_PATH`, falling back to the pre-rename `$MEETILY_DB_PATH`.
pub fn db_path_from_env() -> Option<String> {
    std::env::var(DB_PATH_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var(LEGACY_DB_PATH_ENV).ok())
}

/// Resolve the database path from (in priority order) the `--db` flag, the
/// `PARLEY_DB_PATH` env var, then the platform data dir.
///
/// Pure in its inputs so it can be unit-tested without touching the real
/// environment: `env_override` is what the caller read from `PARLEY_DB_PATH`
/// and `default` is what [`default_db_path`] returned.
pub fn resolve_db_path(
    flag: Option<PathBuf>,
    env_override: Option<String>,
    default: Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(path) = flag {
        return Ok(path);
    }
    // An empty/whitespace-only env var is treated as unset rather than as a
    // path to "", which would otherwise produce a baffling error.
    if let Some(value) = env_override {
        if !value.trim().is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    default.ok_or_else(|| {
        format!(
            "could not determine the platform data directory, so the default \
             Parley database location is unknown.\nPass --db <path> or set \
             ${DB_PATH_ENV} to the location of {DB_FILENAME}."
        )
    })
}

/// Verify the resolved path points at an existing file, with an error message
/// that tells the user what to do about it if not.
pub fn check_db_exists(path: &Path) -> Result<(), String> {
    if path.is_file() {
        return Ok(());
    }
    let hint = if path.exists() {
        "that path exists but is not a file"
    } else {
        "no such file"
    };
    Err(format!(
        "Parley database not found at {} ({hint}).\n\n\
         This server never creates the database — the Parley app owns it. \
         Either:\n  \
         * run Parley once so it creates and migrates the database, or\n  \
         * point this server at an existing database with --db <path> or \
         ${DB_PATH_ENV}.",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_empty_serves_with_defaults() {
        assert_eq!(
            parse_args(Vec::<String>::new()).unwrap(),
            Action::Serve(Args::default())
        );
    }

    #[test]
    fn parse_args_accepts_both_db_forms() {
        let expected = Some(PathBuf::from("/tmp/x.sqlite"));
        match parse_args(["--db", "/tmp/x.sqlite"]).unwrap() {
            Action::Serve(a) => assert_eq!(a.db, expected),
            other => panic!("expected Serve, got {other:?}"),
        }
        match parse_args(["--db=/tmp/x.sqlite"]).unwrap() {
            Action::Serve(a) => assert_eq!(a.db, expected),
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn parse_args_db_without_value_is_an_error() {
        assert!(parse_args(["--db"]).is_err());
    }

    #[test]
    fn parse_args_rejects_unknown_flags() {
        let err = parse_args(["--nope"]).unwrap_err();
        assert!(err.contains("--nope"), "{err}");
    }

    #[test]
    fn parse_args_help_and_version_short_circuit() {
        assert_eq!(parse_args(["--help"]).unwrap(), Action::PrintHelp);
        assert_eq!(parse_args(["-h"]).unwrap(), Action::PrintHelp);
        assert_eq!(parse_args(["--version"]).unwrap(), Action::PrintVersion);
        // Even when combined with otherwise-valid args.
        assert_eq!(
            parse_args(["--db", "/tmp/x.sqlite", "--help"]).unwrap(),
            Action::PrintHelp
        );
    }

    #[test]
    fn resolve_db_path_prefers_flag_then_env_then_default() {
        let flag = PathBuf::from("/flag.sqlite");
        let default = PathBuf::from("/default.sqlite");

        assert_eq!(
            resolve_db_path(
                Some(flag.clone()),
                Some("/env.sqlite".into()),
                Some(default.clone())
            )
            .unwrap(),
            flag
        );
        assert_eq!(
            resolve_db_path(None, Some("/env.sqlite".into()), Some(default.clone())).unwrap(),
            PathBuf::from("/env.sqlite")
        );
        assert_eq!(
            resolve_db_path(None, None, Some(default.clone())).unwrap(),
            default
        );
    }

    #[test]
    fn resolve_db_path_treats_blank_env_as_unset() {
        let default = PathBuf::from("/default.sqlite");
        assert_eq!(
            resolve_db_path(None, Some("   ".into()), Some(default.clone())).unwrap(),
            default
        );
    }

    #[test]
    fn default_db_path_follows_an_unmigrated_legacy_dir() {
        let base = std::env::temp_dir().join(format!("parley-mcp-cli-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        assert_eq!(
            default_db_path_in(&base),
            base.join(APP_IDENTIFIER).join(DB_FILENAME)
        );

        std::fs::create_dir(base.join(LEGACY_APP_IDENTIFIER)).unwrap();
        assert_eq!(
            default_db_path_in(&base),
            base.join(LEGACY_APP_IDENTIFIER).join(DB_FILENAME)
        );

        std::fs::create_dir(base.join(APP_IDENTIFIER)).unwrap();
        assert_eq!(
            default_db_path_in(&base),
            base.join(APP_IDENTIFIER).join(DB_FILENAME)
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_db_path_without_any_source_explains_itself() {
        let err = resolve_db_path(None, None, None).unwrap_err();
        assert!(err.contains(DB_PATH_ENV), "{err}");
    }

    #[test]
    fn check_db_exists_reports_missing_file() {
        let err = check_db_exists(Path::new("/definitely/not/here.sqlite")).unwrap_err();
        assert!(err.contains("no such file"), "{err}");
        assert!(err.contains("--db"), "{err}");
    }

    #[test]
    fn check_db_exists_rejects_a_directory() {
        let err = check_db_exists(Path::new("/tmp")).unwrap_err();
        assert!(err.contains("not a file"), "{err}");
    }
}
