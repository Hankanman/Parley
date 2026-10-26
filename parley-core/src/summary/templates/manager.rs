//! Editable user templates; built-ins remain immutable and can be overridden.
use super::{defaults, validate_and_parse_template, Template};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;

pub fn new_template_id() -> String {
    format!("user_{}", uuid::Uuid::new_v4())
}

#[derive(Clone)]
pub struct TemplateStore {
    root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct TemplateEntry {
    pub id: String,
    pub template: Template,
    pub built_in: bool,
    pub overridden: bool,
}

pub fn validate_id(id: &str) -> Result<(), String> {
    if id.trim().is_empty()
        || id.len() > 250
        || matches!(id, "." | "..")
        || id.chars().any(|c| c == '/' || c == '\\' || c.is_control())
    {
        return Err("Invalid template identifier.".into());
    }
    Ok(())
}

impl TemplateStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn configured() -> Result<Self, String> {
        super::loader::get_custom_templates_dir()
            .map(Self::new)
            .ok_or_else(|| "The templates directory is not ready yet.".into())
    }

    fn path(&self, id: &str) -> Result<PathBuf, String> {
        validate_id(id)?;
        Ok(self.root.join(format!("{id}.json")))
    }

    pub fn get(&self, id: &str) -> Result<TemplateEntry, String> {
        let path = self.path(id)?;
        let built_in = defaults::get_builtin_template(id);
        let overridden = path.try_exists().map_err(|e| e.to_string())?;
        let json = if overridden {
            std::fs::read_to_string(path).map_err(|e| e.to_string())?
        } else {
            built_in
                .ok_or_else(|| "Template not found.".to_string())?
                .to_string()
        };
        Ok(TemplateEntry {
            id: id.into(),
            template: validate_and_parse_template(&json)?,
            built_in: built_in.is_some(),
            overridden,
        })
    }

    pub fn list(&self) -> Result<Vec<TemplateEntry>, String> {
        let mut ids: BTreeSet<String> = defaults::list_builtin_template_ids()
            .into_iter()
            .map(str::to_string)
            .collect();
        match std::fs::read_dir(&self.root) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry.map_err(|e| e.to_string())?.path();
                    if path.extension().is_some_and(|ext| ext == "json") {
                        if let Some(id) = path.file_stem().and_then(|s| s.to_str()) {
                            if validate_id(id).is_ok() {
                                ids.insert(id.into());
                            }
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        Ok(ids
            .into_iter()
            .filter_map(|id| match self.get(&id) {
                Ok(entry) => Some(entry),
                Err(error) => {
                    tracing::warn!("Skipping invalid template {id}: {error}");
                    None
                }
            })
            .collect())
    }

    pub fn save(&self, id: &str, template: &Template) -> Result<(), String> {
        let path = self.path(id)?;
        template.validate()?;
        std::fs::create_dir_all(&self.root).map_err(|e| e.to_string())?;
        // A failed write must not truncate an existing template.
        let mut file = tempfile::NamedTempFile::new_in(&self.root).map_err(|e| e.to_string())?;
        let json = serde_json::to_string_pretty(template).map_err(|e| e.to_string())?;
        file.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
        file.as_file().sync_all().map_err(|e| e.to_string())?;
        file.persist(path).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Delete a user template, or restore an overridden built-in.
    pub fn remove(&self, id: &str) -> Result<(), String> {
        match std::fs::remove_file(self.path(id)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_templates_round_trip_and_builtins_can_be_restored() {
        let dir = tempfile::tempdir().unwrap();
        let store = TemplateStore::new(dir.path().join("templates"));
        let original = store.get("standard_meeting").unwrap();
        let mut edited = original.template.clone();
        edited.name = "Notas de reunión".into();
        store.save("custom_notes", &edited).unwrap();
        assert_eq!(
            store.get("custom_notes").unwrap().template.name,
            edited.name
        );
        store.save("standard_meeting", &edited).unwrap();
        assert!(store.get("standard_meeting").unwrap().overridden);
        assert_eq!(store.list().unwrap().len(), 3);
        store.remove("standard_meeting").unwrap();
        assert_eq!(
            store.get("standard_meeting").unwrap().template.name,
            original.template.name
        );
        store.remove("custom_notes").unwrap();
        assert!(store.get("custom_notes").is_err());
        // Preserve IDs of templates previously created by hand.
        store.save("reunión del equipo.v2", &edited).unwrap();
        assert_eq!(
            store.get("reunión del equipo.v2").unwrap().template.name,
            edited.name
        );
    }

    #[test]
    fn invalid_edits_preserve_the_saved_template_and_paths_stay_inside_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = TemplateStore::new(dir.path().to_path_buf());
        let mut template = store.get("standard_meeting").unwrap().template;
        store.save("custom_notes", &template).unwrap();
        template.name = "   ".into();
        assert!(store.save("custom_notes", &template).is_err());
        assert_ne!(store.get("custom_notes").unwrap().template.name, "   ");
        for id in ["", "../outside", "/tmp/outside", "a/b", "a\\b", "."] {
            assert!(store.get(id).is_err());
            assert!(store.remove(id).is_err());
        }
    }
}
