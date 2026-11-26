//! User-level desktop associations shared by the CLI and the compositor.
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::desktop::{self, DesktopEntry};

pub const WINDOWS_BROWSER: &str = "windows-host";
const HANDLER_PREFIX: &str = "focusstage-handler-";
const WINDOWS_HANDLER: &str = "focusstage-handler-windows-browser.desktop";
const DEFAULTS: &str = "[Default Applications]";
const WEB_SCHEMES: [&str; 2] = ["x-scheme-handler/http", "x-scheme-handler/https"];
const WEB_FILES: [&str; 2] = ["text/html", "application/xhtml+xml"];
const INTERFACE_SCHEMA: &str = "org.gnome.desktop.interface";
const COLOR_SCHEMES: [&str; 3] = ["default", "prefer-light", "prefer-dark"];
const BUILTIN_GTK_THEMES: [&str; 4] = [
    "Adwaita",
    "Adwaita-dark",
    "HighContrast",
    "HighContrastInverse",
];

struct Paths {
    home: PathBuf,
    config: PathBuf,
    data: PathBuf,
}

impl Paths {
    fn user() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let user_dir = |key, suffix| {
            std::env::var_os(key)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(suffix))
        };
        Ok(Self {
            config: user_dir("XDG_CONFIG_HOME", ".config"),
            data: user_dir("XDG_DATA_HOME", ".local/share"),
            home,
        })
    }

    fn settings(&self) -> PathBuf {
        self.config.join("focusstage/settings.json")
    }
    fn mimeapps(&self) -> PathBuf {
        self.config.join("mimeapps.list")
    }
    fn apps(&self) -> PathBuf {
        self.data.join("applications")
    }
    fn gtk3_settings(&self) -> PathBuf {
        self.config.join("gtk-3.0/settings.ini")
    }
    fn gtk4_settings(&self) -> PathBuf {
        self.config.join("gtk-4.0/settings.ini")
    }
    fn icons_default_index(&self) -> PathBuf {
        self.home.join(".icons/default/index.theme")
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct Preferences {
    browser: Option<String>,
    terminal: Option<String>,
    hide_wsl: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeSettingsView {
    pub color_scheme: String,
    pub accent_color: Option<String>,
    pub accent_colors: Vec<String>,
    pub gtk_theme: String,
    pub gtk_themes: Vec<Choice>,
    pub icon_theme: String,
    pub icon_themes: Vec<Choice>,
    pub cursor_theme: String,
    pub cursor_themes: Vec<Choice>,
    pub font_name: String,
    pub fonts: Vec<String>,
    pub text_scaling_factor: f64,
    pub gsettings_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsView {
    pub browser: String,
    pub browsers: Vec<Choice>,
    pub terminal: Option<String>,
    pub terminals: Vec<Choice>,
    pub protocols: BTreeMap<String, String>,
    pub xdg_open_installed: bool,
    pub portal_active: bool,
    pub mimeapps_synced: bool,
    #[serde(default)]
    pub hide_wsl: bool,
    #[serde(default)]
    pub theme: ThemeSettingsView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", content = "id", rename_all = "snake_case")]
pub enum SettingsAction {
    Get,
    InitDefaults,
    SyncHandlers,
    SetBrowser(String),
    SetTerminal(String),
    RemoveHandlers,
    SetColorScheme(String),
    SetAccentColor(String),
    SetGtkTheme(String),
    SetIconTheme(String),
    SetCursorTheme(String),
    SetFontName(String),
    SetTextScale(String),
    ResetTheme,
    SetHideWsl(bool),
}

struct Catalog {
    entries: BTreeMap<String, (PathBuf, DesktopEntry)>,
}

impl Catalog {
    fn read(paths: &Paths) -> Self {
        Self {
            entries: desktop::discover(&desktop::application_dirs(&paths.data)),
        }
    }

    fn entry(&self, id: &str) -> Option<&DesktopEntry> {
        self.entries.get(id).map(|(_, entry)| entry)
    }

    fn choices(&self, test: impl Fn(&DesktopEntry) -> bool) -> Vec<Choice> {
        let mut choices: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, (_, app))| !app.no_display && test(app))
            .map(|(id, (_, app))| Choice {
                id: id.clone(),
                name: app.name_zh_cn.clone().unwrap_or_else(|| app.name.clone()),
            })
            .collect();
        choices.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        choices
    }

    fn browsers(&self) -> Vec<Choice> {
        let mut choices =
            self.choices(|app| app.has_category("WebBrowser") || app.handles(WEB_SCHEMES[1]));
        choices.push(Choice {
            id: WINDOWS_BROWSER.into(),
            name: "Windows 默认浏览器".into(),
        });
        choices
    }

    fn terminals(&self) -> Vec<Choice> {
        self.choices(|app| app.has_category("TerminalEmulator"))
    }
}

pub fn settings(action: SettingsAction) -> Result<SettingsView> {
    let paths = Paths::user()?;
    let catalog = Catalog::read(&paths);
    let mut prefs = read_preferences(&paths)?;
    let mut mimeapps = read_optional(&paths.mimeapps())?;
    let browsers = catalog.browsers();
    let terminals = catalog.terminals();
    let choose_browser = |prefs: &Preferences, text: &str| {
        let valid = |id: &str| browsers.iter().any(|b| b.id == id);
        let defaults = associations(text);
        prefs
            .browser
            .as_deref()
            .filter(|id| valid(id))
            .map(str::to_owned)
            .or_else(|| {
                defaults
                    .get(WEB_SCHEMES[1])
                    .or_else(|| defaults.get(WEB_SCHEMES[0]))
                    .and_then(|list| list.first())
                    .and_then(|desktop| id_from_desktop(desktop))
                    .filter(|id| valid(id))
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| browsers[0].id.clone())
    };
    let mut browser = choose_browser(&prefs, &mimeapps);
    match action {
        SettingsAction::Get => {}
        SettingsAction::RemoveHandlers => {
            mimeapps = remove_defaults(&mimeapps, |id| id.starts_with(HANDLER_PREFIX));
            if paths.mimeapps().exists() {
                write_atomic(&paths.mimeapps(), mimeapps.as_bytes())?;
            }
            if let Ok(entries) = fs::read_dir(paths.apps()) {
                for entry in entries.flatten() {
                    if entry
                        .file_name()
                        .to_str()
                        .is_some_and(|s| s.starts_with(HANDLER_PREFIX) && s.ends_with(".desktop"))
                    {
                        fs::remove_file(entry.path())?;
                    }
                }
                update_database(&paths.apps())?;
            }
        }
        SettingsAction::SetTerminal(id) => {
            let app = catalog
                .entry(&id)
                .filter(|_| terminals.iter().any(|option| option.id == id))
                .context("The selected terminal is not installed")?;
            sync_gsettings_terminal(app)?;
            prefs.terminal = Some(id);
            write_preferences(&paths, &prefs)?;
        }
        SettingsAction::SetColorScheme(value) => {
            apply_color_scheme(&paths, &value)?;
        }
        SettingsAction::SetAccentColor(value) => {
            apply_accent_color(&value)?;
        }
        SettingsAction::SetGtkTheme(id) => {
            apply_gtk_theme(&paths, &id)?;
        }
        SettingsAction::SetIconTheme(id) => {
            apply_icon_theme(&paths, &id)?;
        }
        SettingsAction::SetCursorTheme(id) => {
            apply_cursor_theme(&paths, &id)?;
        }
        SettingsAction::SetFontName(value) => {
            apply_font_name(&paths, &value)?;
        }
        SettingsAction::SetTextScale(value) => {
            apply_text_scale(&value)?;
        }
        SettingsAction::ResetTheme => {
            reset_theme(&paths)?;
        }
        SettingsAction::SetHideWsl(enabled) => {
            prefs.hide_wsl = enabled;
            write_preferences(&paths, &prefs)?;
        }
        action => {
            let overwrite = !matches!(action, SettingsAction::InitDefaults);
            if let SettingsAction::SetBrowser(id) = action {
                if !browsers.iter().any(|option| option.id == id) {
                    bail!("The selected browser is not installed");
                }
                browser = id;
            }
            mimeapps = sync_handlers(&paths, &catalog, &mimeapps, &browser, overwrite)?;
            prefs.browser = Some(browser.clone());
            write_preferences(&paths, &prefs)?;
        }
    }
    let defaults = associations(&mimeapps);
    let protocols = defaults
        .iter()
        .filter_map(|(mime, values)| {
            let scheme = mime.strip_prefix("x-scheme-handler/")?;
            let wrapper = values.first()?.as_str();
            if !wrapper.starts_with(HANDLER_PREFIX) || !paths.apps().join(wrapper).is_file() {
                return None;
            }
            let name = match id_from_desktop(wrapper)? {
                WINDOWS_BROWSER => "Windows 默认浏览器".into(),
                id => catalog.entry(id)?.name.clone(),
            };
            Some((scheme.to_owned(), name))
        })
        .collect();
    let expected = wrapper_id(&browser);
    let mimeapps_synced = paths.apps().join(&expected).is_file()
        && WEB_SCHEMES
            .iter()
            .all(|mime| defaults.get(*mime).and_then(|list| list.first()) == Some(&expected));
    let terminal = prefs
        .terminal
        .filter(|id| terminals.iter().any(|option| &option.id == id))
        .or_else(|| {
            let output = Command::new("gsettings")
                .args([
                    "get",
                    "org.gnome.desktop.default-applications.terminal",
                    "exec",
                ])
                .output()
                .ok()?;
            let exec = std::str::from_utf8(&output.stdout)
                .ok()?
                .trim()
                .trim_matches('\'');
            terminals
                .iter()
                .find(|option| catalog.entry(&option.id).is_some_and(|app| app.cmd == exec))
                .map(|choice| choice.id.clone())
        });
    let theme = read_theme_settings(&paths);
    Ok(SettingsView {
        browser,
        browsers,
        terminal,
        terminals,
        protocols,
        mimeapps_synced,
        xdg_open_installed: on_path("xdg-open"),
        portal_active: Command::new("systemctl")
            .args([
                "--user",
                "is-active",
                "--quiet",
                "xdg-desktop-portal.service",
            ])
            .status()
            .is_ok_and(|s| s.success()),
        hide_wsl: prefs.hide_wsl,
        theme,
    })
}

pub fn hide_wsl_enabled() -> bool {
    Paths::user()
        .and_then(|paths| read_preferences(&paths))
        .is_ok_and(|prefs| prefs.hide_wsl)
}

fn sync_gsettings_terminal(app: &DesktopEntry) -> Result<()> {
    let schema = "org.gnome.desktop.default-applications.terminal";
    let key = "exec";
    let Some(state) = Command::new("gsettings")
        .args(["writable", schema, key])
        .output()
        .ok()
    else {
        return Ok(());
    };
    if state.stdout != b"true\n" {
        return Ok(());
    }
    let result = Command::new("gsettings")
        .args(["set", schema, key, &app.cmd])
        .output()?;
    if !result.status.success() {
        bail!(
            "gsettings could not set the default terminal: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(())
}

fn validate_theme_value(value: &str, label: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.len() > 128
        || trimmed.chars().any(|c| c.is_control() || c == '=')
    {
        bail!("无效的{label}");
    }
    Ok(trimmed.to_owned())
}

fn gsettings_output(args: &[&str]) -> Option<String> {
    let output = Command::new("gsettings").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn gsettings_writable(key: &str) -> bool {
    gsettings_output(&["writable", INTERFACE_SCHEMA, key])
        .is_some_and(|out| out.trim() == "true")
}

fn gsettings_set_interface(key: &str, value: &str) -> Result<bool> {
    if !on_path("gsettings") || !gsettings_writable(key) {
        return Ok(false);
    }
    let output = Command::new("gsettings")
        .args(["set", INTERFACE_SCHEMA, key, value])
        .output()?;
    if !output.status.success() {
        bail!(
            "gsettings 设置 {key} 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(true)
}

fn gsettings_reset_interface(key: &str) -> Result<bool> {
    if !on_path("gsettings") || !gsettings_writable(key) {
        return Ok(false);
    }
    let output = Command::new("gsettings")
        .args(["reset", INTERFACE_SCHEMA, key])
        .output()?;
    if !output.status.success() {
        bail!(
            "gsettings 重置 {key} 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(true)
}

fn parse_gsettings_list(raw: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix(INTERFACE_SCHEMA) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some((key, value)) = rest.split_once(char::is_whitespace) else {
            continue;
        };
        let val = value.trim();
        let parsed = if let Some(inner) = val.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
            inner.replace("\\'", "'")
        } else {
            val.to_owned()
        };
        map.insert(key.to_owned(), parsed);
    }
    map
}

fn parse_gsettings_range(raw: &str) -> Vec<String> {
    let mut lines = raw.lines().map(str::trim);
    if lines.next() != Some("enum") {
        return Vec::new();
    }
    lines
        .filter(|line| !line.is_empty())
        .map(|line| line.trim_matches('\'').to_owned())
        .collect()
}

fn theme_data_roots(paths: &Paths, subdir: &str, legacy_dotdir: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |p: PathBuf| {
        if seen.insert(p.clone()) {
            dirs.push(p);
        }
    };
    push(paths.home.join(legacy_dotdir));
    push(paths.data.join(subdir));
    if let Some(raw) = std::env::var_os("XDG_DATA_DIRS") {
        for base in std::env::split_paths(&raw) {
            if !base.as_os_str().is_empty() {
                push(base.join(subdir));
            }
        }
    }
    for base in ["/usr/local/share", "/usr/share"] {
        push(PathBuf::from(base).join(subdir));
    }
    dirs
}

fn parse_ini_section(text: &str, target_section: &str) -> BTreeMap<String, String> {
    let mut in_section = false;
    let mut map = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_section = line == target_section;
        } else if in_section {
            if let Some((key, val)) = line.split_once('=') {
                map.insert(key.trim().to_owned(), val.trim().to_owned());
            }
        }
    }
    map
}

fn edit_ini_section(
    text: &str,
    target_section: &str,
    mut updates: BTreeMap<String, Option<String>>,
) -> String {
    if updates.is_empty() {
        return text.to_owned();
    }
    let mut output = String::new();
    let mut in_section = false;
    let mut found = false;
    let flush = |output: &mut String, updates: &mut BTreeMap<String, Option<String>>| {
        for (key, value) in std::mem::take(updates) {
            if let Some(val) = value {
                output.push_str(&format!("{key}={val}\n"));
            }
        }
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_section {
                flush(&mut output, &mut updates);
            }
            in_section = trimmed == target_section;
            found |= in_section;
        } else if in_section {
            if let Some((key, _)) = trimmed.split_once('=') {
                if let Some(replacement) = updates.remove(key.trim()) {
                    if let Some(val) = replacement {
                        output.push_str(&format!("{}={val}\n", key.trim()));
                    }
                    continue;
                }
            }
        }
        output.push_str(line);
        output.push('\n');
    }
    if !found && updates.values().any(Option::is_some) {
        if !output.is_empty() && !output.ends_with("\n\n") {
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push('\n');
        }
        output.push_str(target_section);
        output.push('\n');
    }
    flush(&mut output, &mut updates);
    output
}

fn is_valid_gtk_theme_dir(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let has_css = [
        "gtk-3.0/gtk.css",
        "gtk-3.0/gtk-dark.css",
        "gtk-4.0/gtk.css",
        "gtk-4.0/gtk-dark.css",
    ]
    .iter()
    .any(|rel| dir.join(rel).is_file());
    if has_css {
        return true;
    }
    let index = dir.join("index.theme");
    index.is_file()
        && fs::read_to_string(&index).is_ok_and(|content| {
            content.contains("[X-GNOME-Metatheme]") || content.contains("GtkTheme=")
        })
}

fn gtk_theme_supports_gtk4(paths: &Paths, id: &str) -> bool {
    if BUILTIN_GTK_THEMES.contains(&id) {
        return true;
    }
    theme_data_roots(paths, "themes", ".themes")
        .into_iter()
        .any(|root| {
            let dir = root.join(id);
            dir.join("gtk-4.0/gtk.css").is_file() || dir.join("gtk-4.0/gtk-dark.css").is_file()
        })
}

fn discover_gtk_themes_in(dirs: &[PathBuf], current: &str) -> Vec<Choice> {
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    for builtin in BUILTIN_GTK_THEMES {
        found.insert((*builtin).to_owned(), (*builtin).to_owned());
    }
    for root in dirs {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_valid_gtk_theme_dir(&path) {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let display = fs::read_to_string(path.join("index.theme"))
                .ok()
                .and_then(|text| parse_ini_section(&text, "[Desktop Entry]").remove("Name"))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| id.clone());
            found.entry(id).or_insert(display);
        }
    }
    if !current.is_empty() {
        found.entry(current.to_owned()).or_insert_with(|| current.to_owned());
    }
    let mut list: Vec<Choice> = found
        .into_iter()
        .map(|(id, name)| Choice { id, name })
        .collect();
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

fn discover_icon_themes_in(dirs: &[PathBuf], current: &str) -> Vec<Choice> {
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    for root in dirs {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if matches!(id.as_str(), "default" | "hicolor" | "locolor") {
                continue;
            }
            let Ok(index_text) = fs::read_to_string(path.join("index.theme")) else {
                continue;
            };
            let section = parse_ini_section(&index_text, "[Icon Theme]");
            if section.get("Directories").is_none_or(|d| d.trim().is_empty()) {
                continue;
            }
            let hidden = section
                .get("Hidden")
                .is_some_and(|v| v.eq_ignore_ascii_case("true"));
            if hidden && id != "Adwaita" && id != "HighContrast" && id != current {
                continue;
            }
            let display = section
                .get("Name[zh_CN]")
                .or_else(|| section.get("Name"))
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| id.clone());
            found.entry(id).or_insert(display);
        }
    }
    if !current.is_empty() {
        found.entry(current.to_owned()).or_insert_with(|| current.to_owned());
    }
    let mut list: Vec<Choice> = found
        .into_iter()
        .map(|(id, name)| Choice { id, name })
        .collect();
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

fn discover_cursor_themes_in(dirs: &[PathBuf], current: &str) -> Vec<Choice> {
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    for root in dirs {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || !path.join("cursors").is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if id == "default" {
                continue;
            }
            let display = fs::read_to_string(path.join("index.theme"))
                .ok()
                .and_then(|text| {
                    let sec = parse_ini_section(&text, "[Icon Theme]");
                    sec.get("Name[zh_CN]")
                        .or_else(|| sec.get("Name"))
                        .cloned()
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| id.clone());
            found.entry(id).or_insert(display);
        }
    }
    if !current.is_empty() {
        found.entry(current.to_owned()).or_insert_with(|| current.to_owned());
    }
    let mut list: Vec<Choice> = found
        .into_iter()
        .map(|(id, name)| Choice { id, name })
        .collect();
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

fn discover_font_families() -> Vec<String> {
    let Some(output) = Command::new("fc-list")
        .args([":", "family"])
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        for part in line.split(',') {
            let name = part.trim();
            if !name.is_empty() {
                seen.entry(name.to_lowercase())
                    .or_insert_with(|| name.to_owned());
            }
        }
    }
    seen.into_values().collect()
}

fn read_gtk_ini_fallback(paths: &Paths) -> BTreeMap<String, String> {
    let text = read_optional(&paths.gtk3_settings()).unwrap_or_default();
    parse_ini_section(&text, "[Settings]")
}

fn read_theme_settings(paths: &Paths) -> ThemeSettingsView {
    let raw_list = gsettings_output(&["list-recursively", INTERFACE_SCHEMA]);
    let gsettings_available = raw_list.as_ref().is_some_and(|s| !s.trim().is_empty());
    let gmap = raw_list
        .as_deref()
        .map(parse_gsettings_list)
        .unwrap_or_default();
    let ini = read_gtk_ini_fallback(paths);

    let color_scheme = gmap
        .get("color-scheme")
        .filter(|v| COLOR_SCHEMES.contains(&v.as_str()))
        .cloned()
        .unwrap_or_else(|| {
            if ini
                .get("gtk-application-prefer-dark-theme")
                .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
            {
                "prefer-dark".to_owned()
            } else {
                "default".to_owned()
            }
        });

    let accent_colors = if gmap.contains_key("accent-color") || gsettings_writable("accent-color") {
        gsettings_output(&["range", INTERFACE_SCHEMA, "accent-color"])
            .as_deref()
            .map(parse_gsettings_range)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let accent_color = (!accent_colors.is_empty())
        .then(|| gmap.get("accent-color").cloned())
        .flatten();

    let gtk_theme = gmap
        .get("gtk-theme")
        .cloned()
        .or_else(|| ini.get("gtk-theme-name").cloned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Adwaita".to_owned());

    let icon_theme = gmap
        .get("icon-theme")
        .cloned()
        .or_else(|| ini.get("gtk-icon-theme-name").cloned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Adwaita".to_owned());

    let cursor_theme = gmap
        .get("cursor-theme")
        .cloned()
        .or_else(|| ini.get("gtk-cursor-theme-name").cloned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Adwaita".to_owned());

    let font_name = gmap
        .get("font-name")
        .cloned()
        .or_else(|| ini.get("gtk-font-name").cloned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Sans 11".to_owned());

    let text_scaling_factor = gmap
        .get("text-scaling-factor")
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(1.0);

    let theme_dirs = theme_data_roots(paths, "themes", ".themes");
    let icon_dirs = theme_data_roots(paths, "icons", ".icons");

    ThemeSettingsView {
        color_scheme,
        accent_color,
        accent_colors,
        gtk_themes: discover_gtk_themes_in(&theme_dirs, &gtk_theme),
        gtk_theme,
        icon_themes: discover_icon_themes_in(&icon_dirs, &icon_theme),
        icon_theme,
        cursor_themes: discover_cursor_themes_in(&icon_dirs, &cursor_theme),
        cursor_theme,
        font_name,
        fonts: discover_font_families(),
        text_scaling_factor,
        gsettings_available,
    }
}

fn sync_gtk_configs(paths: &Paths, view: &ThemeSettingsView) -> Result<()> {
    let prefer_dark = if view.color_scheme == "prefer-dark" {
        "1"
    } else {
        "0"
    };

    let mut gtk3_updates = BTreeMap::new();
    gtk3_updates.insert(
        "gtk-application-prefer-dark-theme".to_owned(),
        Some(prefer_dark.to_owned()),
    );
    if !view.gtk_theme.is_empty() {
        gtk3_updates.insert("gtk-theme-name".to_owned(), Some(view.gtk_theme.clone()));
    }
    if !view.icon_theme.is_empty() {
        gtk3_updates.insert(
            "gtk-icon-theme-name".to_owned(),
            Some(view.icon_theme.clone()),
        );
    }
    if !view.cursor_theme.is_empty() {
        gtk3_updates.insert(
            "gtk-cursor-theme-name".to_owned(),
            Some(view.cursor_theme.clone()),
        );
    }
    if !view.font_name.is_empty() {
        gtk3_updates.insert("gtk-font-name".to_owned(), Some(view.font_name.clone()));
    }
    let gtk3_cur = read_optional(&paths.gtk3_settings())?;
    let gtk3_next = edit_ini_section(&gtk3_cur, "[Settings]", gtk3_updates);
    if gtk3_next != gtk3_cur {
        write_atomic(&paths.gtk3_settings(), gtk3_next.as_bytes())?;
    }

    let mut gtk4_updates = BTreeMap::new();
    gtk4_updates.insert(
        "gtk-application-prefer-dark-theme".to_owned(),
        Some(prefer_dark.to_owned()),
    );
    if gtk_theme_supports_gtk4(paths, &view.gtk_theme) {
        gtk4_updates.insert("gtk-theme-name".to_owned(), Some(view.gtk_theme.clone()));
    } else {
        gtk4_updates.insert("gtk-theme-name".to_owned(), None);
    }
    if !view.icon_theme.is_empty() {
        gtk4_updates.insert(
            "gtk-icon-theme-name".to_owned(),
            Some(view.icon_theme.clone()),
        );
    }
    if !view.cursor_theme.is_empty() {
        gtk4_updates.insert(
            "gtk-cursor-theme-name".to_owned(),
            Some(view.cursor_theme.clone()),
        );
    }
    if !view.font_name.is_empty() {
        gtk4_updates.insert("gtk-font-name".to_owned(), Some(view.font_name.clone()));
    }
    let gtk4_cur = read_optional(&paths.gtk4_settings())?;
    let gtk4_next = edit_ini_section(&gtk4_cur, "[Settings]", gtk4_updates);
    if gtk4_next != gtk4_cur {
        write_atomic(&paths.gtk4_settings(), gtk4_next.as_bytes())?;
    }

    if !view.cursor_theme.is_empty() {
        let cur_idx = read_optional(&paths.icons_default_index())?;
        let mut icon_updates = BTreeMap::new();
        icon_updates.insert("Inherits".to_owned(), Some(view.cursor_theme.clone()));
        let next_idx = edit_ini_section(&cur_idx, "[Icon Theme]", icon_updates);
        if next_idx != cur_idx {
            write_atomic(&paths.icons_default_index(), next_idx.as_bytes())?;
        }
    }
    Ok(())
}

fn apply_color_scheme(paths: &Paths, value: &str) -> Result<()> {
    let trimmed = value.trim();
    if !COLOR_SCHEMES.contains(&trimmed) {
        bail!("不支持的颜色模式：{trimmed}");
    }
    gsettings_set_interface("color-scheme", trimmed)?;
    let mut view = read_theme_settings(paths);
    view.color_scheme = trimmed.to_owned();
    sync_gtk_configs(paths, &view)
}

fn apply_accent_color(value: &str) -> Result<()> {
    let trimmed = validate_theme_value(value, "强调色")?;
    let supported = gsettings_output(&["range", INTERFACE_SCHEMA, "accent-color"])
        .as_deref()
        .map(parse_gsettings_range)
        .unwrap_or_default();
    if supported.is_empty() {
        bail!("当前 WSL 系统的 GNOME schema 不支持强调色设置");
    }
    if !supported.iter().any(|c| c == &trimmed) {
        bail!("不支持的强调色：{trimmed}");
    }
    if !gsettings_set_interface("accent-color", &trimmed)? {
        bail!("当前系统不允许写入强调色设置");
    }
    Ok(())
}

fn apply_gtk_theme(paths: &Paths, id: &str) -> Result<()> {
    let trimmed = validate_theme_value(id, "GTK 主题")?;
    let current = read_theme_settings(paths);
    if !current.gtk_themes.iter().any(|c| c.id == trimmed) {
        bail!("未找到指定的 GTK 主题：{trimmed}");
    }
    gsettings_set_interface("gtk-theme", &trimmed)?;
    let mut view = read_theme_settings(paths);
    view.gtk_theme = trimmed;
    sync_gtk_configs(paths, &view)
}

fn apply_icon_theme(paths: &Paths, id: &str) -> Result<()> {
    let trimmed = validate_theme_value(id, "图标主题")?;
    let current = read_theme_settings(paths);
    if !current.icon_themes.iter().any(|c| c.id == trimmed) {
        bail!("未找到指定的图标主题：{trimmed}");
    }
    gsettings_set_interface("icon-theme", &trimmed)?;
    let mut view = read_theme_settings(paths);
    view.icon_theme = trimmed;
    sync_gtk_configs(paths, &view)
}

fn apply_cursor_theme(paths: &Paths, id: &str) -> Result<()> {
    let trimmed = validate_theme_value(id, "光标主题")?;
    let current = read_theme_settings(paths);
    if !current.cursor_themes.iter().any(|c| c.id == trimmed) {
        bail!("未找到指定的光标主题：{trimmed}");
    }
    gsettings_set_interface("cursor-theme", &trimmed)?;
    let mut view = read_theme_settings(paths);
    view.cursor_theme = trimmed;
    sync_gtk_configs(paths, &view)
}

fn apply_font_name(paths: &Paths, value: &str) -> Result<()> {
    let trimmed = validate_theme_value(value, "系统字体")?;
    gsettings_set_interface("font-name", &trimmed)?;
    let mut view = read_theme_settings(paths);
    view.font_name = trimmed;
    sync_gtk_configs(paths, &view)
}

fn apply_text_scale(value: &str) -> Result<()> {
    let scale: f64 = value
        .trim()
        .parse()
        .ok()
        .filter(|v: &f64| v.is_finite() && (0.75..=2.0).contains(v))
        .context("文字缩放比例需在 0.75 到 2.00 之间")?;
    let formatted = format!("{scale:.2}");
    if !gsettings_set_interface("text-scaling-factor", &formatted)? {
        bail!("当前系统不支持通过 gsettings 修改文字缩放比例");
    }
    Ok(())
}

fn reset_theme(paths: &Paths) -> Result<()> {
    for key in [
        "color-scheme",
        "accent-color",
        "gtk-theme",
        "icon-theme",
        "cursor-theme",
        "font-name",
        "text-scaling-factor",
    ] {
        let _ = gsettings_reset_interface(key);
    }
    let view = read_theme_settings(paths);
    sync_gtk_configs(paths, &view)
}

fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            fs::metadata(dir.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

fn read_optional(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err).with_context(|| format!("Cannot read {}", path.display())),
    }
}

fn read_preferences(paths: &Paths) -> Result<Preferences> {
    let text = read_optional(&paths.settings())?;
    if text.is_empty() {
        Ok(Preferences::default())
    } else {
        serde_json::from_str(&text).context("Invalid FocusStage settings.json")
    }
}

fn write_preferences(paths: &Paths, prefs: &Preferences) -> Result<()> {
    write_atomic(&paths.settings(), &serde_json::to_vec_pretty(prefs)?)
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().expect("settings path has a parent directory");
    fs::create_dir_all(dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(contents)?;
    temp.persist(path)
        .with_context(|| format!("Cannot write {}", path.display()))?;
    Ok(())
}

fn wrapper_id(id: &str) -> String {
    if id == WINDOWS_BROWSER {
        WINDOWS_HANDLER.to_owned()
    } else {
        format!("{HANDLER_PREFIX}{id}.desktop")
    }
}

fn id_from_desktop(filename: &str) -> Option<&str> {
    if filename == WINDOWS_HANDLER {
        return Some(WINDOWS_BROWSER);
    }
    let id = filename.strip_suffix(".desktop")?;
    let id = id.strip_prefix(HANDLER_PREFIX).unwrap_or(id);
    desktop::valid_id(id).then_some(id)
}

fn valid_mime(mime: &str) -> bool {
    mime.split_once('/').is_some_and(|(kind, ty)| {
        !kind.is_empty()
            && !ty.is_empty()
            && [kind, ty].into_iter().all(|part| {
                part.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-+._".contains(&b))
            })
    })
}

fn associations(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut current = false;
    let mut defaults = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            current = line == DEFAULTS;
        } else if current {
            if let Some((key, value)) = line.split_once('=') {
                if valid_mime(key.trim()) {
                    defaults.insert(
                        key.trim().to_owned(),
                        value
                            .split(';')
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .map(str::to_owned)
                            .collect(),
                    );
                }
            }
        }
    }
    defaults
}

fn edit_defaults(mimeapps: &str, mut updates: BTreeMap<String, Vec<String>>) -> String {
    if updates.is_empty() {
        return mimeapps.to_owned();
    }
    let mut output = String::new();
    let mut in_defaults = false;
    let mut found = false;
    let flush = |output: &mut String, updates: &mut BTreeMap<String, Vec<String>>| {
        for (key, values) in std::mem::take(updates) {
            emit_default(output, &key, &values);
        }
    };
    for line in mimeapps.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if in_defaults {
                flush(&mut output, &mut updates);
            }
            in_defaults = trimmed == DEFAULTS;
            found |= in_defaults;
        } else if in_defaults {
            if let Some((key, _)) = trimmed.split_once('=') {
                if let Some(values) = updates.remove(key.trim()) {
                    emit_default(&mut output, key.trim(), &values);
                    continue;
                }
            }
        }
        output.push_str(line);
        output.push('\n');
    }
    if !found && !updates.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(DEFAULTS);
        output.push('\n');
    }
    flush(&mut output, &mut updates);
    output
}

fn emit_default(output: &mut String, key: &str, values: &[String]) {
    if !values.is_empty() {
        output.push_str(&format!("{key}={};\n", values.join(";")));
    }
}

fn remove_defaults(text: &str, remove: impl Fn(&str) -> bool) -> String {
    let updates = associations(text)
        .into_iter()
        .filter_map(|(mime, mut list)| {
            let count = list.len();
            list.retain(|id| !remove(id));
            (list.len() != count).then_some((mime, list))
        })
        .collect();
    edit_defaults(text, updates)
}

fn sync_handlers(
    paths: &Paths,
    catalog: &Catalog,
    text: &str,
    browser: &str,
    overwrite: bool,
) -> Result<String> {
    let apps = paths.apps();
    let mut schemes: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut wrappers = BTreeMap::new();
    let web = WEB_SCHEMES.into_iter().chain(WEB_FILES).collect::<Vec<_>>();
    for (id, (_, app)) in &catalog.entries {
        let mut mimes: Vec<_> = app
            .mime_types
            .iter()
            .map(String::as_str)
            .filter(|mime| valid_mime(mime))
            .collect();
        if id == browser {
            mimes.extend(web.iter().copied());
        }
        if mimes.is_empty() {
            continue;
        }
        mimes.sort_unstable();
        mimes.dedup();
        wrappers.insert(
            wrapper_id(id),
            (id.as_str(), app.name.as_str(), mimes.clone()),
        );
        for mime in mimes
            .into_iter()
            .filter(|mime| mime.starts_with("x-scheme-handler/") && !web.contains(mime))
        {
            schemes.entry(mime).or_default().push(id);
        }
    }
    wrappers.insert(
        WINDOWS_HANDLER.into(),
        (WINDOWS_BROWSER, "Windows Default Browser", web.clone()),
    );
    for (filename, (id, name, mimes)) in &wrappers {
        let mime_types = mimes.join(";");
        let content = format!("[Desktop Entry]\nType=Application\nName={name} (FocusStage)\nNoDisplay=true\nTerminal=false\nExec=/usr/local/bin/focusstage-open --handler {id} -- %U\nMimeType={mime_types};\n");
        write_atomic(&apps.join(filename), content.as_bytes())?;
    }
    if let Ok(entries) = fs::read_dir(&apps) {
        for entry in entries.flatten() {
            if entry.file_name().to_str().is_some_and(|name| {
                name.starts_with(HANDLER_PREFIX)
                    && name.ends_with(".desktop")
                    && !wrappers.contains_key(name)
            }) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    update_database(&apps)?;

    let defaults = associations(text);
    let selected = wrapper_id(browser);
    let mut updates = BTreeMap::new();
    let mut register = |mime: &str, handler: &str, replace: bool| {
        let mut list = defaults.get(mime).cloned().unwrap_or_default();
        if !replace
            && list
                .first()
                .is_some_and(|first| id_from_desktop(first) != id_from_desktop(handler))
        {
            return;
        }
        // Keep existing non-FocusStage choices as fallbacks, including for uninstallation.
        list.retain(|candidate| !candidate.starts_with(HANDLER_PREFIX));
        list.insert(0, handler.to_owned());
        updates.insert(mime.to_owned(), list);
    };
    for mime in web {
        register(mime, &selected, overwrite);
    }
    for (mime, ids) in schemes {
        let installed = defaults
            .get(mime)
            .and_then(|list| list.first())
            .and_then(|desktop| id_from_desktop(desktop))
            .filter(|id| ids.contains(id));
        if let Some(id) = installed.or_else(|| (ids.len() == 1).then(|| ids[0])) {
            register(mime, &wrapper_id(id), false);
        }
    }
    let output = edit_defaults(text, updates);
    if output != text {
        write_atomic(&paths.mimeapps(), output.as_bytes())?;
    }
    Ok(output)
}

fn update_database(dir: &Path) -> Result<()> {
    match Command::new("update-desktop-database").arg(dir).output() {
        Ok(output) if !output.status.success() => bail!(
            "Could not update the desktop database: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err.into()),
        _ => {}
    }
    Ok(())
}

pub enum OpenRoute {
    WindowsBrowser,
    Linux {
        id: String,
        app: String,
        wm_class: Option<String>,
        args: Vec<String>,
    },
}

pub struct OpenTarget {
    uri: String,
    path: Option<String>,
    scheme: String,
}

impl OpenTarget {
    pub fn parse(value: &str) -> Result<Self> {
        if value.is_empty() || value.chars().any(char::is_control) {
            bail!("Invalid URI or path");
        }
        if value.starts_with('/') {
            return Ok(Self {
                uri: file_uri(value),
                path: Some(value.into()),
                scheme: "file".into(),
            });
        }
        let (scheme, rest) = value
            .split_once(':')
            .context("Expected a URI or an absolute local path")?;
        if !scheme
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
            || !scheme
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || b"-+.".contains(&ch))
        {
            bail!("Invalid URI scheme");
        }
        let scheme = scheme.to_ascii_lowercase();
        let path = if scheme == "file" {
            let rest = rest
                .strip_prefix("//localhost/")
                .map(|p| format!("/{p}"))
                .unwrap_or_else(|| rest.strip_prefix("//").unwrap_or(rest).to_owned());
            if !rest.starts_with('/') {
                bail!("Only local file:// URIs are supported");
            }
            Some(decode_path(&rest)?)
        } else {
            if matches!(scheme.as_str(), "http" | "https") && !valid_web_url(value) {
                bail!("Invalid HTTP(S) URL");
            }
            None
        };
        Ok(Self {
            uri: value.to_owned(),
            path,
            scheme,
        })
    }

    pub fn is_web(&self) -> bool {
        matches!(self.scheme.as_str(), "http" | "https")
    }
    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub(crate) fn local_path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    fn mime_type(&self) -> Result<String> {
        let Some(path) = &self.path else {
            return Ok(format!("x-scheme-handler/{}", self.scheme));
        };
        let info = Command::new("gio")
            .args(["info", "-a", "standard::content-type", "--", path])
            .output()?;
        let text = std::str::from_utf8(&info.stdout)?;
        text.lines()
            .find_map(|line| line.trim().strip_prefix("standard::content-type: "))
            .map(str::to_owned)
            .context("Could not identify the local file's MIME type")
    }
}

pub fn preferred_terminal_command() -> Option<String> {
    if let Ok(paths) = Paths::user() {
        let catalog = Catalog::read(&paths);
        let prefs = read_preferences(&paths).ok();
        let terminals = catalog.terminals();
        let chosen_id = prefs
            .and_then(|p| p.terminal)
            .filter(|id| terminals.iter().any(|t| &t.id == id))
            .or_else(|| terminals.first().map(|t| t.id.clone()));
        if let Some(id) = chosen_id {
            if let Some(entry) = catalog.entry(&id) {
                return Some(entry.cmd.clone());
            }
        }
    }
    [
        "ptyxis",
        "gnome-terminal",
        "xfce4-terminal",
        "konsole",
        "alacritty",
        "kitty",
        "wezterm",
        "tilix",
        "foot",
        "xterm",
    ]
    .into_iter()
    .find(|bin| on_path(bin))
    .map(str::to_owned)
}

pub fn root_terminal_launch(focusstage_bin: &Path) -> Result<(String, Vec<String>, Option<String>, bool)> {
    let mut terminal_cmd: Option<String> = None;
    let mut wm_class: Option<String> = None;

    if let Ok(view) = settings(SettingsAction::Get) {
        let target_id = view
            .terminal
            .or_else(|| view.terminals.first().map(|c| c.id.clone()));
        if let Some(id) = target_id {
            if let Ok(paths) = Paths::user() {
                let catalog = Catalog::read(&paths);
                if let Some(entry) = catalog.entry(&id) {
                    terminal_cmd = Some(entry.cmd.clone());
                    wm_class = Some(format!("{}{}", crate::desktop::WRAPPER_PREFIX, id));
                }
            }
        }
    }

    let cmd = terminal_cmd
        .or_else(preferred_terminal_command)
        .context("未在 WSL 中找到可用的 Linux 终端程序。")?;
    let base = Path::new(&cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&cmd)
        .to_ascii_lowercase();

    let focusstage_str = focusstage_bin.to_string_lossy().into_owned();
    let pkexec_args = [
        "/usr/bin/pkexec".to_string(),
        "--disable-internal-agent".to_string(),
        focusstage_str,
        "root-shell".to_string(),
    ];

    let mut args = Vec::new();
    let mut run_as_root = false;
    match base.as_str() {
        "ptyxis" => {
            args.extend(["-s".into(), "--title".into(), "Root 终端".into(), "--".into()]);
            args.extend(pkexec_args);
        }
        "gnome-terminal" | "tilix" | "kitty" | "foot" => {
            args.push("--".into());
            args.extend(pkexec_args);
        }
        "wezterm" => {
            args.extend(["start".into(), "--".into()]);
            args.extend(pkexec_args);
        }
        "xfce4-terminal" | "konsole" | "xterm" | "alacritty" | "terminator" => {
            args.push("-e".into());
            args.extend(pkexec_args);
        }
        _ => {
            run_as_root = true;
        }
    }

    Ok((cmd, args, wm_class, run_as_root))
}

pub fn valid_web_url(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && !rest.contains('\\')
        && !rest.chars().any(|c| c.is_control() || c.is_whitespace())
        && rest
            .split(['/', '?', '#'])
            .next()
            .is_some_and(|authority| !authority.is_empty())
}

pub fn resolve(target: &OpenTarget, handler: Option<&str>) -> Result<OpenRoute> {
    let paths = Paths::user()?;
    let catalog = Catalog::read(&paths);
    let mime = target.mime_type()?;
    let mimeapps = read_optional(&paths.mimeapps())?;
    let defaults = associations(&mimeapps);
    let stored = defaults
        .get(&mime)
        .into_iter()
        .flatten()
        .filter_map(|id| id_from_desktop(id));
    let prefs = read_preferences(&paths)?;
    let browsers = catalog.browsers();
    let candidates = if let Some(id) = handler {
        vec![id]
    } else if target.is_web() {
        prefs
            .browser
            .as_deref()
            .into_iter()
            .chain(stored)
            .chain(browsers.iter().map(|b| b.id.as_str()))
            .collect()
    } else {
        stored
            .chain(
                catalog
                    .entries
                    .iter()
                    .filter_map(|(id, (_, app))| app.handles(&mime).then_some(id.as_str())),
            )
            .collect()
    };
    for id in candidates {
        if id == WINDOWS_BROWSER {
            if target.is_web() {
                return Ok(OpenRoute::WindowsBrowser);
            }
            if WEB_FILES.contains(&mime.as_str()) && target.path.is_some() {
                if let Some(linux) = browsers
                    .iter()
                    .find(|b| b.id != WINDOWS_BROWSER)
                    .and_then(|b| catalog.entry(&b.id).map(|app| (b.id.as_str(), app)))
                {
                    return Ok(linux_route(linux.0, linux.1, target));
                }
            }
            continue;
        }
        if let Some(app) = catalog.entry(id) {
            if app.handles(&mime)
                || (target.is_web() && app.has_category("WebBrowser"))
                || (WEB_FILES.contains(&mime.as_str()) && app.has_category("WebBrowser"))
            {
                return Ok(linux_route(id, app, target));
            }
        }
    }
    bail!("No installed handler for {mime}")
}

fn linux_route(id: &str, app: &DesktopEntry, target: &OpenTarget) -> OpenRoute {
    OpenRoute::Linux {
        id: id.into(),
        app: app.cmd.clone(),
        wm_class: app.wm_class.clone(),
        args: app.open_args(&target.uri, target.path.as_deref()),
    }
}

pub(crate) fn file_uri(path: &str) -> String {
    let mut out = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn decode_path(path: &str) -> Result<String> {
    let mut bytes = Vec::new();
    let mut remaining = path.as_bytes();
    while let Some((&byte, rest)) = remaining.split_first() {
        if byte == b'%' {
            let hex = rest.get(..2).context("Invalid file URI encoding")?;
            bytes.push(
                u8::from_str_radix(std::str::from_utf8(hex)?, 16)
                    .context("Invalid file URI encoding")?,
            );
            remaining = &rest[2..];
        } else {
            bytes.push(byte);
            remaining = rest;
        }
    }
    if bytes.contains(&0) {
        bail!("Invalid local path");
    }
    Ok(String::from_utf8(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_other_mimeapps_sections_and_uninstall_fallbacks() {
        let input = "# comment\n[Default Applications]\nx-scheme-handler/https=old.desktop;\ntext/plain=edit.desktop;\n\n[Added Associations]\nx-scheme-handler/https=other.desktop;\n";
        let updated = edit_defaults(
            input,
            BTreeMap::from([(
                "x-scheme-handler/https".into(),
                vec![
                    "focusstage-handler-new.desktop".into(),
                    "old.desktop".into(),
                ],
            )]),
        );
        assert!(
            updated.contains("x-scheme-handler/https=focusstage-handler-new.desktop;old.desktop;")
        );
        assert_eq!(
            remove_defaults(&updated, |id| id.starts_with(HANDLER_PREFIX)),
            input
        );
    }

    #[test]
    fn windows_open_accepts_only_absolute_web_urls() {
        for url in ["https://example.invalid/a?b=c", "HTTP://127.0.0.1:1234/"] {
            assert!(valid_web_url(url));
        }
        for url in [
            "file:///etc/passwd",
            "editor://callback",
            "https:example.invalid",
            "https:///a",
            "https://a\\b",
            "http://a\n",
        ] {
            assert!(!valid_web_url(url));
        }
    }

    #[test]
    fn local_paths_are_encoded_but_not_given_to_windows() {
        let target = OpenTarget::parse("/tmp/a #1.html").unwrap();
        assert_eq!(target.uri(), "file:///tmp/a%20%231.html");
        assert_eq!(OpenTarget::parse(target.uri()).unwrap().path, target.path);
        assert!(!target.is_web());
        assert!(OpenTarget::parse("file://example.invalid/a").is_err());
    }

    #[test]
    fn parses_gsettings_list_and_range() {
        let raw = "\
org.gnome.desktop.interface color-scheme 'prefer-dark'
org.gnome.desktop.interface accent-color 'teal'
org.gnome.desktop.interface text-scaling-factor 1.25
org.gnome.desktop.interface font-name 'Noto Sans CJK SC 11'
";
        let map = parse_gsettings_list(raw);
        assert_eq!(map.get("color-scheme").map(String::as_str), Some("prefer-dark"));
        assert_eq!(map.get("accent-color").map(String::as_str), Some("teal"));
        assert_eq!(map.get("text-scaling-factor").map(String::as_str), Some("1.25"));
        assert_eq!(map.get("font-name").map(String::as_str), Some("Noto Sans CJK SC 11"));

        let range = "enum\n'blue'\n'teal'\n'purple'\n";
        assert_eq!(parse_gsettings_range(range), vec!["blue", "teal", "purple"]);
    }

    #[test]
    fn edits_gtk_ini_section_preserving_unrelated_entries() {
        let input = "# comment\n[Settings]\ngtk-theme-name=Adwaita\ngtk-enable-animations=1\n\n[Custom]\nfoo=bar\n";
        let mut updates = BTreeMap::new();
        updates.insert("gtk-theme-name".to_owned(), Some("Yaru".to_owned()));
        updates.insert("gtk-application-prefer-dark-theme".to_owned(), Some("1".to_owned()));
        updates.insert("gtk-enable-animations".to_owned(), None);
        let edited = edit_ini_section(input, "[Settings]", updates);
        assert!(edited.contains("# comment\n"));
        assert!(edited.contains("gtk-theme-name=Yaru\n"));
        assert!(edited.contains("gtk-application-prefer-dark-theme=1\n"));
        assert!(!edited.contains("gtk-enable-animations"));
        assert!(edited.contains("[Custom]\nfoo=bar\n"));

        let created = edit_ini_section(
            "",
            "[Settings]",
            BTreeMap::from([("gtk-application-prefer-dark-theme".to_owned(), Some("1".to_owned()))]),
        );
        assert_eq!(created, "[Settings]\ngtk-application-prefer-dark-theme=1\n");
    }

    #[test]
    fn discovers_gtk_icon_and_cursor_themes_without_false_positives() {
        let tmp = tempfile::tempdir().unwrap();
        let themes_dir = tmp.path().join("themes");
        let icons_dir = tmp.path().join("icons");
        fs::create_dir_all(themes_dir.join("Default/gtk-3.0")).unwrap();
        fs::write(themes_dir.join("Default/gtk-3.0/gtk-keys.css"), "/* keys */").unwrap();
        fs::create_dir_all(themes_dir.join("CustomGtk/gtk-3.0")).unwrap();
        fs::write(themes_dir.join("CustomGtk/gtk-3.0/gtk.css"), "/* css */").unwrap();

        let gtk_themes = discover_gtk_themes_in(&[themes_dir], "Adwaita");
        assert!(gtk_themes.iter().any(|c| c.id == "CustomGtk"));
        assert!(!gtk_themes.iter().any(|c| c.id == "Default"));
        assert!(gtk_themes.iter().any(|c| c.id == "Adwaita"));

        fs::create_dir_all(icons_dir.join("hicolor")).unwrap();
        fs::write(icons_dir.join("hicolor/index.theme"), "[Icon Theme]\nName=Hicolor\nDirectories=16x16\n").unwrap();
        fs::create_dir_all(icons_dir.join("Papirus")).unwrap();
        fs::write(icons_dir.join("Papirus/index.theme"), "[Icon Theme]\nName=Papirus\nDirectories=16x16\n").unwrap();
        fs::create_dir_all(icons_dir.join("Bibata/cursors")).unwrap();
        fs::write(icons_dir.join("Bibata/index.theme"), "[Icon Theme]\nName=Bibata Modern\n").unwrap();

        let icon_themes = discover_icon_themes_in(std::slice::from_ref(&icons_dir), "Adwaita");
        assert!(icon_themes.iter().any(|c| c.id == "Papirus" && c.name == "Papirus"));
        assert!(!icon_themes.iter().any(|c| c.id == "hicolor"));
        assert!(!icon_themes.iter().any(|c| c.id == "Bibata"));

        let cursor_themes = discover_cursor_themes_in(&[icons_dir], "Adwaita");
        assert!(cursor_themes.iter().any(|c| c.id == "Bibata" && c.name == "Bibata Modern"));
        assert!(!cursor_themes.iter().any(|c| c.id == "Papirus"));
    }
}
