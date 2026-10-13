pub mod manager;
pub mod models;
pub mod repositories;
pub mod search;
pub mod setup;

#[cfg(test)]
mod migration_tests {
    use std::collections::HashMap;

    /// Two branches each adding a migration with the same timestamp merge
    /// without a textual conflict, then every startup fails with
    /// "UNIQUE constraint failed: _sqlx_migrations.version" (happened with
    /// the FTS5 and meeting-chat migrations). Catch it at test time with the
    /// colliding file names.
    #[test]
    fn migration_versions_are_unique() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
        let mut seen: HashMap<String, String> = HashMap::new();
        for entry in std::fs::read_dir(dir).expect("read migrations dir") {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            let Some((version, _)) = name.split_once('_') else {
                continue;
            };
            if let Some(other) = seen.insert(version.to_string(), name.clone()) {
                panic!("migrations share version {version}: {other} and {name}");
            }
        }
        assert!(!seen.is_empty(), "no migrations found in {dir}");
    }
}
