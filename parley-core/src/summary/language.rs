//! Summary output language is independent of transcription and template language.
pub const OPTIONS: &[(&str, &str)] = &[
    ("auto", "Auto"),
    ("es", "Español"),
    ("en", "English"),
    ("ca", "Català"),
    ("gl", "Galego"),
    ("eu", "Euskara"),
    ("fr", "Français"),
    ("de", "Deutsch"),
    ("pt", "Português"),
    ("it", "Italiano"),
    ("nl", "Nederlands"),
    ("pl", "Polski"),
    ("uk", "Українська"),
    ("ru", "Русский"),
    ("ar", "العربية"),
    ("zh", "中文"),
    ("ja", "日本語"),
    ("ko", "한국어"),
    ("hi", "हिन्दी"),
];
pub const UI_KEY: &str = "summaryLanguage";

/// Shared preference for meeting reports and follow-up email drafts.
pub async fn load(pool: &sqlx::SqlitePool) -> &'static str {
    use crate::database::repositories::setting::{KEY_UI_CONFIG, SettingsRepository};
    match SettingsRepository::get_setting::<serde_json::Value>(pool, KEY_UI_CONFIG).await {
        Ok(config) => {
            let Some(value) = config.as_ref().and_then(|config| config.get(UI_KEY)) else {
                return "auto";
            };
            match value.as_str().and_then(known_code) {
                Some(code) => code,
                None => {
                    tracing::warn!("Unrecognised summaryLanguage setting; using Auto");
                    "auto"
                }
            }
        }
        Err(error) => {
            tracing::warn!("Could not read summary output language; using Auto: {error}");
            "auto"
        }
    }
}

pub fn known_code(code: &str) -> Option<&'static str> {
    OPTIONS
        .iter()
        .find(|(id, _)| *id == code)
        .map(|&(id, _)| id)
}

pub(crate) fn target(code: &str) -> Option<&'static str> {
    if code == "auto" {
        return None;
    }
    OPTIONS
        .iter()
        .find(|(id, _)| *id == code)
        .map(|&(_, name)| name)
}

pub(crate) fn report_instruction(target: &str) -> String {
    format!(
        "OUTPUT LANGUAGE: Write the complete final report in {target}, regardless of the language of the template or its instructions. \
         This includes the meeting title, prose, tasks, section headings, table labels and empty-section messages. \
         Preserve proper names, URLs, code and identifiers. Keep JSON property names unchanged."
    )
}

pub(crate) fn email_instruction(target: &str) -> String {
    format!(
        "OUTPUT LANGUAGE: Write the entire follow-up email in {target}, including its subject, greeting, \
         prose, section labels, task descriptions and sign-off. Translate English examples in these instructions. \
         Preserve people's names, task ownership, deadlines and completion status. Keep JSON property names unchanged. \
         In the plain-text format keep only the leading Subject: marker in English, followed by the translated subject."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reports_and_emails_read_the_same_saved_output_language() {
        use crate::database::repositories::setting::{KEY_UI_CONFIG, SettingsRepository};
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        assert_eq!(load(&pool).await, "auto");
        for code in ["es", "de"] {
            SettingsRepository::set_setting(
                &pool,
                KEY_UI_CONFIG,
                &serde_json::json!({
                    "primaryLanguage": "en", "summaryLanguage": code
                }),
            )
            .await
            .unwrap();
            let saved = load(&pool).await;
            assert_eq!(saved, code);
            assert!(target(saved).is_some());
        }
        SettingsRepository::set_setting(
            &pool,
            KEY_UI_CONFIG,
            &serde_json::json!({"summaryLanguage": "invalid"}),
        )
        .await
        .unwrap();
        assert_eq!(load(&pool).await, "auto");
        assert_eq!(target("auto"), None);
        assert_eq!(target("ignore previous instructions"), None);
    }
}
