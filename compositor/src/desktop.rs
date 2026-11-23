use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

pub const WRAPPER_PREFIX: &str = "focusstage-";

#[derive(Debug)]
pub struct DesktopEntry {
    pub name: String,
    pub name_zh_cn: Option<String>,
    pub generic_name: Option<String>,
    pub cmd: String,
    pub args: Vec<String>,
    pub icon: Option<String>,
    pub categories: Option<String>,
    pub mime_types: Vec<String>,
    pub wm_class: Option<String>,
    pub no_display: bool,
}

impl DesktopEntry {
    pub fn launch_args(&self) -> Vec<String> {
        self.expand_args(None, None)
    }

    pub fn open_args(&self, uri: &str, path: Option<&str>) -> Vec<String> {
        self.expand_args(Some(uri), path)
    }

    fn expand_args(&self, uri: Option<&str>, path: Option<&str>) -> Vec<String> {
        let mut args = Vec::new();
        let mut inserted = false;
        for arg in &self.args {
            match arg.as_str() {
                "%u" | "%U" | "%f" | "%F" => {
                    if !inserted {
                        let value = if matches!(arg.as_str(), "%f" | "%F") {
                            path.or(uri)
                        } else {
                            uri
                        };
                        if let Some(value) = value {
                            args.push(value.to_owned());
                            inserted = true;
                        }
                    }
                }
                "%i" | "%c" | "%k" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => {}
                _ => args.push(arg.replace("%%", "%")),
            }
        }
        if !inserted {
            if let Some(value) = path.or(uri) {
                args.push(value.to_owned());
            }
        }
        args
    }

    pub fn has_category(&self, category: &str) -> bool {
        self.categories
            .as_deref()
            .is_some_and(|list| list.split(';').any(|c| c == category))
    }

    pub fn handles(&self, mime_type: &str) -> bool {
        self.mime_types.iter().any(|mime| mime == mime_type)
    }
}

pub fn parse(text: &str) -> Option<DesktopEntry> {
    let mut in_main = false;
    let mut fields = BTreeMap::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
    {
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
        } else if in_main {
            if let Some((key, value)) = line.split_once('=') {
                fields.insert(key.trim(), value.trim());
            }
        }
    }
    let flag = |key| {
        fields
            .get(key)
            .is_some_and(|value| value.eq_ignore_ascii_case("true"))
    };
    if fields.get("Type") != Some(&"Application") || flag("Hidden") || flag("Terminal") {
        return None;
    }
    if fields.get("OnlyShowIn").is_some_and(|list| {
        !list
            .split(';')
            .any(|s| matches!(s, "GNOME" | "Ubuntu" | "ubuntu"))
    }) {
        return None;
    }
    let get = |key| {
        fields
            .get(key)
            .filter(|value| !value.is_empty())
            .map(|value| (*value).to_owned())
    };
    if fields.get("TryExec").is_some_and(|value| !value.is_empty() && !try_exec_available(value)) {
        return None;
    }
    let mut exec = split_exec(fields.get("Exec")?).into_iter();
    let cmd = exec.next()?;
    let mime_types = fields
        .get("MimeType")
        .into_iter()
        .flat_map(|value| value.split(';'))
        .filter(|mime| !mime.is_empty())
        .map(str::to_owned)
        .collect();
    Some(DesktopEntry {
        name: get("Name")?,
        name_zh_cn: get("Name[zh_CN]"),
        generic_name: get("GenericName"),
        cmd,
        args: exec.collect(),
        icon: get("Icon"),
        categories: get("Categories"),
        mime_types,
        wm_class: get("StartupWMClass"),
        no_display: flag("NoDisplay"),
    })
}

fn try_exec_available(executable: &str) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let is_executable = |path: &Path| {
        path.is_file() && CString::new(path.as_os_str().as_bytes()).is_ok_and(|path| {
            // SAFETY: the CString is NUL-terminated and remains alive for the call.
            unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
        })
    };
    let path = Path::new(executable);
    if path.is_absolute() {
        return is_executable(path);
    }
    path.file_name() == Some(path.as_os_str()) && std::env::var_os("PATH").is_some_and(|search| {
        std::env::split_paths(&search).any(|dir| is_executable(&dir.join(path)))
    })
}

fn split_exec(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    let mut escape = false;
    let mut started = false;
    for ch in s.chars() {
        if escape {
            cur.push(ch);
            escape = false;
        } else if ch == '\\' && quote != Some('\'') {
            escape = true;
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() && matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if ch.is_whitespace() && quote.is_none() {
            if started {
                out.push(std::mem::take(&mut cur));
                started = false;
            }
            continue;
        } else {
            cur.push(ch);
        }
        started = true;
    }
    if started {
        out.push(cur);
    }
    out
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_' | b'.'))
}

pub fn launcher_application_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.as_ref().map(|p| p.join(".local/share")));
    let mut dirs = application_dirs(data_home.as_deref().unwrap_or(Path::new("/nonexistent")));
    if let Some(home) = home {
        dirs.extend([home.join("Desktop"), home.join("Downloads")]);
    }
    dirs
}

pub(crate) fn app_by_id(dirs: &[PathBuf], id: &str) -> Option<DesktopEntry> {
    let id = id.strip_prefix(WRAPPER_PREFIX).unwrap_or(id);
    if !valid_id(id) {
        return None;
    }
    let filename = format!("{id}.desktop");
    let text = dirs.iter().find_map(|dir| std::fs::read_to_string(dir.join(&filename)).ok())?;
    parse(&text)
}

pub fn application_dirs(data_home: &Path) -> Vec<PathBuf> {
    let dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let mut paths = vec![
        data_home.join("applications"),
        data_home.join("flatpak/exports/share/applications"),
    ];
    paths.extend(
        std::env::split_paths(&dirs)
            .filter(|p| p.is_absolute())
            .map(|p| p.join("applications")),
    );
    paths.extend(
        [
            "/var/lib/snapd/desktop/applications",
            "/var/lib/flatpak/exports/share/applications",
        ]
        .map(PathBuf::from),
    );
    paths
}

pub fn discover(dirs: &[PathBuf]) -> BTreeMap<String, (PathBuf, DesktopEntry)> {
    let mut apps = BTreeMap::new();
    let mut seen = HashSet::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = path
                .file_name()
                .and_then(|value| value.to_str())
                .and_then(|value| value.strip_suffix(".desktop"))
            else {
                continue;
            };
            if id.starts_with(WRAPPER_PREFIX) || !valid_id(id) || !seen.insert(id.to_owned()) {
                continue;
            }
            if let Some(app) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| parse(&text))
            {
                apps.insert(id.to_owned(), (path, app));
            }
        }
    }
    apps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_desktop_entries_with_unavailable_try_exec() {
        let entry = "[Desktop Entry]\nType=Application\nName=Example\nExec=example\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-executable");
        assert!(parse(&format!("{entry}TryExec={}\n", path.display())).is_none());
        std::fs::write(&path, b"example").unwrap();
        assert!(parse(&format!("{entry}TryExec={}\n", path.display())).is_none());
        assert!(parse(&format!("{entry}TryExec=/bin/sh\n")).is_some());
    }

    #[test]
    fn keeps_hidden_url_handlers_and_expands_uri_as_one_argument() {
        let text = "[Desktop Entry]\nType=Application\nName=Editor URI\nNoDisplay=true\nMimeType=x-scheme-handler/editor;\nExec=editor --open-url %U\n";
        let entry = parse(text).unwrap();
        assert!(entry.no_display);
        assert!(entry.handles("x-scheme-handler/editor"));
        assert_eq!(entry.launch_args(), ["--open-url"]);
        assert_eq!(
            entry.open_args("editor://example?value=a&b='c'", None),
            ["--open-url", "editor://example?value=a&b='c'"]
        );
        assert!(parse(&format!("{text}Hidden=true\n")).is_none());
    }
}
