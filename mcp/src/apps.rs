use std::path::PathBuf;

use focusstage_compositor::desktop::{self, DesktopEntry};

pub struct DiscoveredDesktopApp {
    pub stem: String,
    pub wrapper_id: String,
    pub source: &'static str,
    pub file_path: Option<String>,
    pub parsed: DesktopEntry,
}

impl DiscoveredDesktopApp {
    pub(crate) fn is_launchable(&self) -> bool {
        matches!(self.source, "desktop" | "appimage")
    }

    pub(crate) fn matches_name(&self, matches: impl Fn(&str) -> bool) -> bool {
        matches(&self.parsed.name) || self.parsed.name_zh_cn.as_deref().is_some_and(matches)
    }
}

pub(crate) fn resolve_app<'a>(
    apps: &'a [DiscoveredDesktopApp],
    id: Option<&str>,
    name: Option<&str>,
) -> Result<&'a DiscoveredDesktopApp, String> {
    let name = name.map(str::to_lowercase);
    let matches: Vec<_> = apps.iter().filter(|app| {
        app.is_launchable() && match id {
            Some(id) => app.wrapper_id == id,
            None => name.as_ref().is_some_and(|name| app.matches_name(|label| label.to_lowercase() == *name)),
        }
    }).collect();
    match matches.as_slice() {
        [app] => Ok(app),
        [] => Err("Application not found; use stage_list_apps to find its ID or exact name".into()),
        apps => Err(format!("Ambiguous application name; pass an app_id: {}",
            apps.iter().map(|app| app.wrapper_id.as_str()).collect::<Vec<_>>().join(", "))),
    }
}

fn detect_windows_user_dir_for_apps() -> Option<PathBuf> {
    if let Ok(entries) = std::fs::read_dir("/mnt/c/Users") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if matches!(s.as_ref(), "Public" | "Default" | "Default User" | "All Users") {
                continue;
            }
            let path = entry.path();
            if path.join("Downloads").is_dir() && path.join("Desktop").is_dir() {
                return Some(path);
            }
        }
    }
    None
}

pub fn discover_desktop_apps() -> Vec<DiscoveredDesktopApp> {
    let scan_dirs = desktop::launcher_application_dirs();
    let mut download_dirs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            download_dirs.push(PathBuf::from(&home).join("Downloads"));
        }
    }
    if let Some(win_home) = detect_windows_user_dir_for_apps() {
        download_dirs.push(win_home.join("Downloads"));
    }

    let mut apps = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    for (stem, (path, parsed)) in desktop::discover(&scan_dirs) {
        seen_ids.insert(stem.clone());
        if parsed.no_display {
            continue;
        }
        let wrapper_id = format!("focusstage-{stem}");
        apps.push(DiscoveredDesktopApp {
            stem,
            wrapper_id,
            source: "desktop",
            file_path: Some(path.display().to_string()),
            parsed,
        });
    }

    // Discover browser-downloaded .AppImage and .deb files
    for dir in &download_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let lower = fname.to_ascii_lowercase();
            let (source, ext_len) = if lower.ends_with(".appimage") {
                ("appimage", ".appimage".len())
            } else if lower.ends_with(".deb") {
                ("deb", ".deb".len())
            } else {
                continue;
            };
            let raw_stem = &fname[..fname.len() - ext_len];
            let safe_stem: String = raw_stem
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
                .collect();
            let id_key = format!("{source}-{safe_stem}");
            if !seen_ids.insert(id_key.clone()) {
                continue;
            }
            let full = path.display().to_string();
            apps.push(DiscoveredDesktopApp {
                stem: id_key.clone(),
                wrapper_id: format!("focusstage-{id_key}"),
                source,
                file_path: Some(full.clone()),
                parsed: DesktopEntry {
                    name: raw_stem.to_string(),
                    name_zh_cn: None,
                    generic_name: Some(if source == "appimage" {
                        "便携应用 (AppImage)".to_string()
                    } else {
                        "安装包 (.deb)".to_string()
                    }),
                    cmd: full,
                    args: Vec::new(),
                    icon: None,
                    categories: Some("Downloads;".to_string()),
                    mime_types: Vec::new(),
                    wm_class: None,
                    no_display: false,
                },
            });
        }
    }

    apps.sort_by(|a, b| a.parsed.name.to_lowercase().cmp(&b.parsed.name.to_lowercase()));
    apps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str, source: &'static str) -> DiscoveredDesktopApp {
        let parsed = desktop::parse(&format!(
            "[Desktop Entry]\nType=Application\nName={name}\nName[zh_CN]=代码编辑器\nExec=/snap/bin/code --force-user-env %F\n"
        )).unwrap();
        DiscoveredDesktopApp {
            stem: id.into(), wrapper_id: id.into(), source, file_path: None, parsed,
        }
    }

    #[test]
    fn resolves_exact_names_and_ids_without_guessing_executables() {
        let entries = [
            entry("code-stable", "Visual Studio Code", "desktop"),
            entry("code-preview", "Visual Studio Code", "desktop"),
            entry("code-package", "Other", "deb"),
        ];
        let app = resolve_app(&entries, Some("code-stable"), None).unwrap();
        assert_eq!(app.parsed.cmd, "/snap/bin/code");
        assert_eq!(app.parsed.launch_args(), ["--force-user-env"]);
        assert!(resolve_app(&entries[..1], None, Some("visual studio code")).is_ok());
        assert!(resolve_app(&entries[..1], None, Some("代码编辑器")).is_ok());
        let ambiguous = resolve_app(&entries, None, Some("Visual Studio Code")).err().unwrap();
        assert!(ambiguous.contains("code-stable") && ambiguous.contains("code-preview"));
        assert!(resolve_app(&entries, None, Some("code")).is_err());
        assert!(resolve_app(&entries, Some("code-package"), None).is_err());
    }
}
