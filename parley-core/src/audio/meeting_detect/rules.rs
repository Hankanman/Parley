//! Pure matching: does a PipeWire stream node's property map belong to a
//! known meeting app? No PipeWire types here — the watcher flattens a
//! node's info props (plus its owning client's props) into a plain
//! `HashMap<String, String>` and asks [`classify`].
//!
//! ## What counts as a call
//!
//! Only **microphone capture** streams: `media.class = Stream/Input/Audio`
//! that are *not* sink-monitor captures (`stream.capture.sink = true` means
//! "record what's playing", e.g. OBS or Parley's own system-audio stream).
//! A meeting app opens one of these exactly when it joins a call with the
//! mic live, and closes it when the call ends.
//!
//! ## Native apps vs browsers
//!
//! Native/Electron apps (Teams, Zoom, Slack, Discord, Webex, Jitsi Meet) are
//! matched by a case-insensitive substring of any of the identifying
//! properties (see [`IDENTITY_KEYS`]) — the binary name is the most
//! reliable one, since Electron apps frequently report
//! `application.name = "Chromium"`.
//!
//! Browsers (Chrome, Chromium, Firefox, Brave, Edge) don't expose which tab
//! or site opened the stream, so a web call (Meet, Teams web, Zoom web) is
//! detected as "a browser is capturing the microphone" — in practice a
//! browser only does that for a WebRTC `getUserMedia` call. We report it as
//! e.g. "Google Chrome" with `is_browser = true` rather than guessing the
//! site. To keep unknown Electron apps (which also say "Chromium") from
//! being reported as browser calls, a browser match requires the **binary**
//! to be a browser whenever `application.process.binary` is present; the
//! application name is only used as a fallback when the binary is missing.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A meeting app a stream was attributed to. `id` is stable (used to key
/// debouncing and in event payloads); `name` is for display.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MeetingApp {
    pub id: String,
    pub name: String,
    /// A browser capturing the mic (a web call in some tab), rather than a
    /// dedicated meeting app.
    pub is_browser: bool,
}

struct Rule {
    id: &'static str,
    name: &'static str,
    /// Lower-case substrings matched against every identity property.
    needles: &'static [&'static str],
}

/// Dedicated meeting apps. Checked before browsers, so an Electron app
/// that calls itself "Chromium" is still attributed by its binary.
const APP_RULES: &[Rule] = &[
    Rule {
        id: "teams",
        name: "Microsoft Teams",
        // teams-for-linux (binary + flatpak id `...teams_for_linux`), the
        // retired official client (`teams`), and `msteams`.
        needles: &[
            "teams-for-linux",
            "teams_for_linux",
            "msteams",
            "microsoft teams",
            "teams",
        ],
    },
    Rule {
        id: "zoom",
        name: "Zoom",
        // Binary `zoom`, app name "ZOOM VoiceEngine", flatpak `us.zoom.Zoom`.
        needles: &["zoom"],
    },
    Rule {
        id: "slack",
        name: "Slack",
        needles: &["slack"],
    },
    Rule {
        id: "discord",
        name: "Discord",
        // Also Vesktop / WebCord, common Linux Discord clients.
        needles: &["discord", "vesktop", "webcord"],
    },
    Rule {
        id: "webex",
        name: "Webex",
        needles: &["webex", "ciscocollabhost"],
    },
    Rule {
        id: "jitsi",
        name: "Jitsi Meet",
        needles: &["jitsi"],
    },
];

struct BrowserRule {
    id: &'static str,
    name: &'static str,
    /// Exact (lower-case) binary names.
    binaries: &'static [&'static str],
    /// Lower-case substrings of `application.name`, used only when the
    /// binary is unknown.
    app_names: &'static [&'static str],
}

const BROWSER_RULES: &[BrowserRule] = &[
    BrowserRule {
        id: "chrome",
        name: "Google Chrome",
        binaries: &["chrome", "google-chrome", "google-chrome-stable"],
        app_names: &["google chrome"],
    },
    BrowserRule {
        id: "chromium",
        name: "Chromium",
        binaries: &["chromium", "chromium-browser"],
        app_names: &["chromium"],
    },
    BrowserRule {
        id: "firefox",
        name: "Firefox",
        binaries: &["firefox", "firefox-bin", "firefox-esr"],
        app_names: &["firefox"],
    },
    BrowserRule {
        id: "brave",
        name: "Brave",
        binaries: &["brave", "brave-browser"],
        app_names: &["brave"],
    },
    BrowserRule {
        id: "edge",
        name: "Microsoft Edge",
        binaries: &["msedge", "microsoft-edge", "microsoft-edge-stable"],
        app_names: &["microsoft edge"],
    },
];

/// Properties that identify the app behind a stream. `application.*` comes
/// from the stream itself (or its client, merged in by the watcher);
/// `pipewire.access.portal.app_id` / `application.id` carry the flatpak id.
pub const IDENTITY_KEYS: &[&str] = &[
    "application.name",
    "application.process.binary",
    "application.id",
    "pipewire.access.portal.app_id",
    "node.name",
    "media.name",
];

/// The only node class we care about: an app capturing audio.
pub const CAPTURE_STREAM_CLASS: &str = "Stream/Input/Audio";

/// Attribute a stream node to a meeting app, or `None` if it isn't a mic
/// capture by a known meeting app. `own_pid` is this process's pid, so
/// Parley's own capture streams are never counted.
pub fn classify(props: &HashMap<String, String>, own_pid: u32) -> Option<MeetingApp> {
    if props.get("media.class").map(String::as_str) != Some(CAPTURE_STREAM_CLASS) {
        return None;
    }
    if props
        .get("stream.capture.sink")
        .is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1")
    {
        return None;
    }
    if is_own_stream(props, own_pid) {
        return None;
    }

    let lowered: Vec<String> = IDENTITY_KEYS
        .iter()
        .filter_map(|key| props.get(*key))
        .map(|v| v.to_lowercase())
        .collect();

    for rule in APP_RULES {
        if lowered
            .iter()
            .any(|value| rule.needles.iter().any(|needle| value.contains(needle)))
        {
            return Some(MeetingApp {
                id: rule.id.to_string(),
                name: rule.name.to_string(),
                is_browser: false,
            });
        }
    }

    let binary = props
        .get("application.process.binary")
        .map(|b| b.to_lowercase());
    let app_name = props.get("application.name").map(|n| n.to_lowercase());
    for rule in BROWSER_RULES {
        let matched = match &binary {
            Some(binary) => rule.binaries.contains(&binary.as_str()),
            None => app_name
                .as_deref()
                .is_some_and(|name| rule.app_names.iter().any(|n| name.contains(n))),
        };
        if matched {
            return Some(MeetingApp {
                id: rule.id.to_string(),
                name: rule.name.to_string(),
                is_browser: true,
            });
        }
    }

    None
}

/// Parley's own capture streams: same process, or our stream/app names
/// (`pw::run_capture_loop` sets `node.name = parley-capture`,
/// `application.name = Parley`).
fn is_own_stream(props: &HashMap<String, String>, own_pid: u32) -> bool {
    let own = own_pid.to_string();
    if props.get("application.process.id") == Some(&own)
        || props.get("pipewire.sec.pid") == Some(&own)
    {
        return true;
    }
    if props
        .get("node.name")
        .is_some_and(|n| n.to_lowercase().starts_with("parley"))
    {
        return true;
    }
    props
        .get("application.name")
        .is_some_and(|n| n.eq_ignore_ascii_case("parley"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN_PID: u32 = 4242;

    fn stream(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        let mut props: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        props
            .entry("media.class".into())
            .or_insert_with(|| CAPTURE_STREAM_CLASS.into());
        props
    }

    fn id_of(props: &HashMap<String, String>) -> Option<String> {
        classify(props, OWN_PID).map(|app| app.id)
    }

    #[test]
    fn matches_teams_for_linux_even_when_it_calls_itself_chromium() {
        let props = stream(&[
            ("application.name", "Chromium"),
            ("application.process.binary", "teams-for-linux"),
        ]);
        let app = classify(&props, OWN_PID).unwrap();
        assert_eq!(app.id, "teams");
        assert_eq!(app.name, "Microsoft Teams");
        assert!(!app.is_browser);
    }

    #[test]
    fn matches_each_native_app() {
        let cases = [
            (
                vec![
                    ("application.name", "ZOOM VoiceEngine"),
                    ("application.process.binary", "zoom"),
                ],
                "zoom",
            ),
            (vec![("application.process.binary", "slack")], "slack"),
            (
                vec![
                    ("application.name", "WEBRTC VoiceEngine"),
                    ("application.process.binary", "Discord"),
                ],
                "discord",
            ),
            (
                vec![("application.process.binary", "CiscoCollabHost")],
                "webex",
            ),
            (vec![("application.name", "Jitsi Meet")], "jitsi"),
            (
                vec![("pipewire.access.portal.app_id", "us.zoom.Zoom")],
                "zoom",
            ),
            (
                vec![(
                    "application.id",
                    "com.github.IsmaelMartinez.teams_for_linux",
                )],
                "teams",
            ),
        ];
        for (pairs, expected) in cases {
            assert_eq!(
                id_of(&stream(&pairs)).as_deref(),
                Some(expected),
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn matches_browsers_as_web_calls() {
        let cases = [
            ("chrome", "Google Chrome", "chrome"),
            ("chromium-browser", "Chromium", "chromium"),
            ("firefox", "Firefox", "firefox"),
            ("brave", "Brave", "brave"),
            ("msedge", "Microsoft Edge", "edge"),
        ];
        for (binary, app_name, expected) in cases {
            let props = stream(&[
                ("application.name", app_name),
                ("application.process.binary", binary),
            ]);
            let app = classify(&props, OWN_PID).unwrap();
            assert_eq!(app.id, expected);
            assert!(app.is_browser);
        }
    }

    #[test]
    fn browser_falls_back_to_app_name_when_binary_is_missing() {
        let props = stream(&[("application.name", "Firefox")]);
        assert_eq!(id_of(&props).as_deref(), Some("firefox"));
    }

    #[test]
    fn unknown_electron_app_calling_itself_chromium_is_not_a_browser_call() {
        let props = stream(&[
            ("application.name", "Chromium"),
            ("application.process.binary", "some-electron-app"),
        ]);
        assert_eq!(id_of(&props), None);
    }

    #[test]
    fn ignores_playback_streams_and_devices() {
        for class in ["Stream/Output/Audio", "Audio/Source", "Video/Source"] {
            let props = stream(&[
                ("media.class", class),
                ("application.process.binary", "zoom"),
            ]);
            assert_eq!(id_of(&props), None, "{class}");
        }
    }

    #[test]
    fn ignores_sink_monitor_captures() {
        let props = stream(&[
            ("application.process.binary", "zoom"),
            ("stream.capture.sink", "true"),
        ]);
        assert_eq!(id_of(&props), None);
    }

    #[test]
    fn ignores_parleys_own_streams() {
        let by_pid = stream(&[
            ("application.process.binary", "chrome"),
            ("application.process.id", "4242"),
        ]);
        assert_eq!(id_of(&by_pid), None);
        let by_client_pid = stream(&[
            ("application.process.binary", "chrome"),
            ("pipewire.sec.pid", "4242"),
        ]);
        assert_eq!(id_of(&by_client_pid), None);
        let by_name = stream(&[("node.name", "parley-capture"), ("media.name", "zoom")]);
        assert_eq!(id_of(&by_name), None);
        let by_app = stream(&[("application.name", "Parley"), ("media.name", "zoom")]);
        assert_eq!(id_of(&by_app), None);
    }

    #[test]
    fn ignores_unrelated_capture_streams() {
        let props = stream(&[
            ("application.name", "Audacity"),
            ("application.process.binary", "audacity"),
        ]);
        assert_eq!(id_of(&props), None);
    }
}
