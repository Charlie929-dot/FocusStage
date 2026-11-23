//! User-level desktop associations shared by the CLI and the compositor.
use std::collections::BTreeMap;
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

struct Paths {
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
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct Preferences {
    browser: Option<String>,
    terminal: Option<String>,
}

#[derive(Serialize)]
pub struct Choice {
    id: String,
    name: String,
}

#[derive(Serialize)]
pub struct SettingsView {
    browser: String,
    browsers: Vec<Choice>,
    terminal: Option<String>,
    terminals: Vec<Choice>,
    protocols: BTreeMap<String, String>,
    xdg_open_installed: bool,
    portal_active: bool,
    mimeapps_synced: bool,
}

pub enum SettingsAction {
    Get,
    InitDefaults,
    SyncHandlers,
    SetBrowser(String),
    SetTerminal(String),
    RemoveHandlers,
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
    })
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

fn file_uri(path: &str) -> String {
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
}
