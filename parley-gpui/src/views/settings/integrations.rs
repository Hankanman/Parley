//! "Integrations" settings page: MCP server info and a ready-to-paste
//! client config snippet, mirroring `frontend/src/components/McpSettings.tsx`
//! against the same Tauri-free core (`parley_core::mcp_config`).

use gpui_kit::component::{
    button::Button,
    clipboard::Clipboard,
    h_flex, v_flex,
    label::Label,
    setting::{SettingGroup, SettingItem, SettingPage},
    ActiveTheme, Icon,
};
use gpui_kit::*;

use super::state::SettingsCache;
use super::SettingsView;

pub fn page(view: &Entity<SettingsView>, cx: &mut Context<SettingsView>) -> SettingPage {
    let _ = cx;
    let view = view.clone();

    SettingPage::new("Integrations")
        .icon(Icon::new(gpui_kit::assets::IconName::Plug))
        .group(
            SettingGroup::new()
                .title("Model Context Protocol (MCP)")
                .description(
                    "parley-mcp is a separate binary an AI client (Claude Desktop, Claude Code) \
                     spawns by absolute path to read this app's meeting database directly.",
                )
                .item(info_item(&view)),
        )
}

fn info_item(view: &Entity<SettingsView>) -> SettingItem {
    let _ = view;
    SettingItem::render(move |_options, _window, cx| {
        let info = SettingsCache::global(cx).mcp_info.clone();

        let Some(info) = info else {
            return Label::new("Could not determine the MCP server configuration.").into_any_element();
        };

        let db_line = if info.db_is_default {
            "Database: default location (no --db flag needed)".to_string()
        } else {
            format!("Database: {}", info.db_path)
        };
        let db_exists = if info.db_exists { "" } else { " (not created yet)" };

        let binary_line = match &info.binary_path {
            Some(path) if info.binary_found => format!("Binary found: {}", path),
            Some(path) => format!("Binary not found at last known path: {}", path),
            None => "Binary not found — build parley-mcp or set $PARLEY_MCP_BIN".to_string(),
        };
        let reveal_path = info.binary_path.clone().filter(|_| info.binary_found);

        let bin_display = info
            .binary_path
            .clone()
            .unwrap_or_else(|| "/absolute/path/to/parley-mcp".to_string());

        let claude_code_snippet = claude_code_snippet(&bin_display, &info.db_path, info.db_is_default);
        let json_snippet = json_snippet(&bin_display, &info.db_path, info.db_is_default);

        v_flex()
            .w_full()
            .gap_3()
            .child(Label::new(format!("{}{}", db_line, db_exists)).text_color(cx.theme().muted_foreground))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        Label::new(binary_line).text_color(if info.binary_found {
                            cx.theme().success
                        } else {
                            cx.theme().danger
                        }),
                    )
                    .children(reveal_path.map(|path| {
                        Button::new("mcp-reveal-binary")
                            .outline()
                            .label("Show in file manager")
                            .on_click(move |_, _, _cx| {
                                if let Err(e) = parley_core::mcp_config::reveal_mcp_binary(path.clone()) {
                                    log::warn!("settings: failed to reveal MCP binary: {}", e);
                                }
                            })
                    })),
            )
            .child(snippet_block(cx, "Claude Code", &claude_code_snippet))
            .child(snippet_block(cx, "Claude Desktop / other MCP JSON config", &json_snippet))
            .into_any_element()
    })
}

/// The `claude mcp add` one-liner for registering `parley-mcp` with Claude
/// Code. Omits `--db` when `db_path` is the platform default (the MCP
/// server resolves the same default on its own).
fn claude_code_snippet(binary_path: &str, db_path: &str, db_is_default: bool) -> String {
    if db_is_default {
        format!("claude mcp add parley -- {}", binary_path)
    } else {
        format!("claude mcp add parley -- {} --db {}", binary_path, db_path)
    }
}

/// The `mcpServers` JSON block for Claude Desktop / other MCP clients that
/// read a JSON config file. Omits the `--db` arg under the same condition
/// as `claude_code_snippet`.
fn json_snippet(binary_path: &str, db_path: &str, db_is_default: bool) -> String {
    if db_is_default {
        format!(
            "{{\n  \"mcpServers\": {{\n    \"parley\": {{\n      \"command\": \"{}\"\n    }}\n  }}\n}}",
            binary_path
        )
    } else {
        format!(
            "{{\n  \"mcpServers\": {{\n    \"parley\": {{\n      \"command\": \"{}\",\n      \"args\": [\"--db\", \"{}\"]\n    }}\n  }}\n}}",
            binary_path, db_path
        )
    }
}

fn snippet_block(cx: &App, title: &'static str, snippet: &str) -> impl IntoElement {
    let snippet = SharedString::from(snippet.to_string());
    v_flex()
        .w_full()
        .gap_1()
        .child(Label::new(title).text_xs())
        .child(
            h_flex()
                .w_full()
                .items_start()
                .justify_between()
                .gap_2()
                .p_2()
                .rounded_md()
                .bg(cx.theme().muted)
                .child(
                    div()
                        .flex_1()
                        .font_family("monospace")
                        .text_xs()
                        .child(snippet.clone()),
                )
                .child(Clipboard::new(SharedString::from(format!("copy-{}", title))).value(snippet)),
        )
}

#[cfg(test)]
mod tests {
    use super::{claude_code_snippet, json_snippet};

    #[test]
    fn claude_code_snippet_omits_db_flag_for_the_default_path() {
        let snippet = claude_code_snippet("/bin/parley-mcp", "/data/meeting_minutes.sqlite", true);
        assert_eq!(snippet, "claude mcp add parley -- /bin/parley-mcp");
    }

    #[test]
    fn claude_code_snippet_includes_db_flag_for_a_non_default_path() {
        let snippet = claude_code_snippet("/bin/parley-mcp", "/custom/db.sqlite", false);
        assert_eq!(
            snippet,
            "claude mcp add parley -- /bin/parley-mcp --db /custom/db.sqlite"
        );
    }

    #[test]
    fn json_snippet_omits_args_for_the_default_path() {
        let snippet = json_snippet("/bin/parley-mcp", "/data/meeting_minutes.sqlite", true);
        assert!(snippet.contains("\"command\": \"/bin/parley-mcp\""));
        assert!(!snippet.contains("args"));
    }

    #[test]
    fn json_snippet_includes_args_for_a_non_default_path() {
        let snippet = json_snippet("/bin/parley-mcp", "/custom/db.sqlite", false);
        assert!(snippet.contains("\"args\": [\"--db\", \"/custom/db.sqlite\"]"));
    }
}
