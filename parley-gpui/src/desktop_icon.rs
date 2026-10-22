//! Keeps Parley's launcher icon from being mistaken for KDE's Parley.
//!
//! KDE ships an unrelated vocabulary trainer called Parley, and the Breeze
//! icon theme has an icon named `parley` for it. AppImage integrators such
//! as Gear Lever write the launcher entry as `Icon=<dir>/parley`, and some
//! launchers (Plasma's AppGrid) resolve that by its base name through the
//! icon theme first — so Parley showed KDE Parley's icon.
//!
//! At startup this installs the app icon under the app ID in the user's
//! hicolor theme, and repoints any of Parley's own launcher entries
//! (recognised by `StartupWMClass`) whose icon is named `parley` at that ID.
//! Integrators rewrite the entry on every update, so it runs every launch;
//! both steps are no-ops once things are right. Failures are logged and
//! otherwise ignored.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const APP_ID: &str = "io.github.hankanman.Parley";
const ICON_PNG: &[u8] = include_bytes!("../packaging/linux/icon.png");
const ICON_LINE: &str = "Icon=io.github.hankanman.Parley";
const WM_CLASS_LINE: &str = "StartupWMClass=io.github.hankanman.Parley";

/// Run the fix-up on a background thread (plain file I/O, off the UI).
pub fn spawn() {
    let Some(data_dir) = dirs::data_dir() else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("desktop-icon".into())
        .spawn(move || {
            if let Err(e) = install_icon(&data_dir) {
                log::warn!("desktop icon: failed to install the app icon: {}", e);
            }
            if let Err(e) = fix_launcher_entries(&data_dir.join("applications")) {
                log::warn!("desktop icon: failed to check launcher entries: {}", e);
            }
        });
    if let Err(e) = spawned {
        log::warn!("desktop icon: failed to start: {}", e);
    }
}

/// Write the app icon to `<data>/icons/hicolor/512x512/apps/<APP_ID>.png`
/// unless it's already there.
fn install_icon(data_dir: &Path) -> io::Result<()> {
    let dir = data_dir.join("icons/hicolor/512x512/apps");
    let path = dir.join(format!("{APP_ID}.png"));
    if fs::read(&path).is_ok_and(|existing| existing == ICON_PNG) {
        return Ok(());
    }
    fs::create_dir_all(&dir)?;
    write_atomically(&path, ICON_PNG)?;
    log::info!("desktop icon: installed {}", path.display());
    Ok(())
}

/// Repoint Parley's launcher entries in `applications_dir` at the app-ID
/// icon. Other apps' entries are never touched.
fn fix_launcher_entries(applications_dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(applications_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "desktop") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(fixed) = fix_desktop_entry(&contents) {
            write_atomically(&path, fixed.as_bytes())?;
            log::info!("desktop icon: set {} to Icon={}", path.display(), APP_ID);
        }
    }
    Ok(())
}

/// The corrected text of a launcher entry, or `None` if it isn't Parley's
/// or its icon doesn't collide.
fn fix_desktop_entry(contents: &str) -> Option<String> {
    if !contents.lines().any(|line| line.trim() == WM_CLASS_LINE) {
        return None;
    }
    let mut changed = false;
    let mut fixed: Vec<&str> = Vec::new();
    for line in contents.lines() {
        match line.strip_prefix("Icon=") {
            Some(icon) if collides(icon.trim()) => {
                fixed.push(ICON_LINE);
                changed = true;
            }
            _ => fixed.push(line),
        }
    }
    if !changed {
        return None;
    }
    let mut out = fixed.join("\n");
    if contents.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

/// Whether an `Icon=` value would be looked up as the theme's `parley`
/// icon: its file name, minus any extension, is `parley`.
fn collides(icon: &str) -> bool {
    let name = Path::new(icon)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(icon);
    name.eq_ignore_ascii_case("parley")
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = PathBuf::from(path);
    tmp.as_mut_os_string().push(".parley-tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::{collides, fix_desktop_entry, fix_launcher_entries, install_icon, APP_ID, ICON_PNG};
    use std::fs;

    const GEAR_LEVER_ENTRY: &str = "[Desktop Entry]\n\
        Type=Application\n\
        Name=Parley\n\
        Icon=/home/u/AppImages/.icons/parley\n\
        Exec=env DESKTOPINTEGRATION=1 /home/u/AppImages/parley.appimage\n\
        StartupWMClass=io.github.hankanman.Parley\n";

    #[test]
    fn repoints_a_colliding_icon() {
        let fixed = fix_desktop_entry(GEAR_LEVER_ENTRY).expect("should be rewritten");
        assert!(fixed.contains("\nIcon=io.github.hankanman.Parley\n"));
        assert!(!fixed.contains(".icons/parley"));
        assert!(fixed.contains("Exec=env DESKTOPINTEGRATION=1 /home/u/AppImages/parley.appimage\n"));
        assert!(fixed.ends_with('\n'));
    }

    #[test]
    fn leaves_correct_or_foreign_entries_alone() {
        let already = GEAR_LEVER_ENTRY.replace("/home/u/AppImages/.icons/parley", APP_ID);
        assert_eq!(fix_desktop_entry(&already), None);
        // KDE's own Parley: same icon name, different app.
        let kde = "[Desktop Entry]\nName=Parley\nIcon=parley\nExec=parley\n";
        assert_eq!(fix_desktop_entry(kde), None);
    }

    #[test]
    fn collision_ignores_directory_and_extension() {
        assert!(collides("parley"));
        assert!(collides("/x/.icons/parley.png"));
        assert!(!collides("/x/.icons/parley-recordings"));
        assert!(!collides(APP_ID));
    }

    #[test]
    fn fixes_files_on_disk_and_installs_icon() {
        let dir = std::env::temp_dir().join(format!("parley-desktop-icon-{}", std::process::id()));
        let apps = dir.join("applications");
        fs::create_dir_all(&apps).unwrap();
        fs::write(apps.join("parley.desktop"), GEAR_LEVER_ENTRY).unwrap();
        let other = "[Desktop Entry]\nName=Other\nIcon=/x/.icons/parley\n";
        fs::write(apps.join("other.desktop"), other).unwrap();

        fix_launcher_entries(&apps).unwrap();
        install_icon(&dir).unwrap();

        let fixed = fs::read_to_string(apps.join("parley.desktop")).unwrap();
        assert!(fixed.contains("\nIcon=io.github.hankanman.Parley\n"));
        assert_eq!(fs::read_to_string(apps.join("other.desktop")).unwrap(), other);
        let icon = dir.join(format!("icons/hicolor/512x512/apps/{APP_ID}.png"));
        assert_eq!(fs::read(icon).unwrap(), ICON_PNG);
        fs::remove_dir_all(&dir).unwrap();
    }
}
