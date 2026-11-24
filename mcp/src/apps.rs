use focusstage_compositor::control::ControlClient;
use focusstage_compositor::desktop;
use focusstage_compositor::protocol::{ControlRequest, ControlResponse, DesktopAppInfo};

pub(crate) fn resolve_app<'a>(
    apps: &'a [DesktopAppInfo],
    id: Option<&str>,
    name: Option<&str>,
) -> Result<&'a DesktopAppInfo, String> {
    let name = name.map(str::to_lowercase);
    let matches: Vec<_> = apps
        .iter()
        .filter(|app| {
            app.is_launchable()
                && match id {
                    Some(id) => app.id == id,
                    None => name
                        .as_ref()
                        .is_some_and(|name| app.matches_name(|label| label.to_lowercase() == *name)),
                }
        })
        .collect();
    match matches.as_slice() {
        [app] => Ok(app),
        [] => Err("Application not found; use stage_list_apps to find its ID or exact name".into()),
        apps => Err(format!(
            "Ambiguous application name; pass an app_id: {}",
            apps.iter()
                .map(|app| app.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

pub fn discover_desktop_apps() -> Vec<DesktopAppInfo> {
    desktop::discover_launcher_apps(false)
}

pub fn discover_desktop_apps_via_daemon(client: Option<&ControlClient>) -> Vec<DesktopAppInfo> {
    if let Some(client) = client {
        if let Ok(ControlResponse::Apps { apps, .. }) = client.call(&ControlRequest::ListApps) {
            return apps;
        }
    }
    discover_desktop_apps()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str, source: &'static str) -> DesktopAppInfo {
        let parsed = desktop::parse(&format!(
            "[Desktop Entry]\nType=Application\nName={name}\nName[zh_CN]=代码编辑器\nExec=/snap/bin/code --force-user-env %F\n"
        ))
        .unwrap();
        DesktopAppInfo {
            id: id.into(),
            stem: id.into(),
            source: source.into(),
            file_path: None,
            name: parsed.name.clone(),
            name_zh_cn: parsed.name_zh_cn.clone(),
            generic_name: parsed.generic_name.clone(),
            cmd: parsed.cmd.clone(),
            args: parsed.launch_args(),
            wm_class: id.into(),
            icon: parsed.icon.clone(),
            icon_path: None,
            categories: parsed.categories,
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
        assert_eq!(app.cmd, "/snap/bin/code");
        assert_eq!(app.args, ["--force-user-env"]);
        assert!(resolve_app(&entries[..1], None, Some("visual studio code")).is_ok());
        assert!(resolve_app(&entries[..1], None, Some("代码编辑器")).is_ok());
        let ambiguous = resolve_app(&entries, None, Some("Visual Studio Code"))
            .err()
            .unwrap();
        assert!(ambiguous.contains("code-stable") && ambiguous.contains("code-preview"));
        assert!(resolve_app(&entries, None, Some("code")).is_err());
        assert!(resolve_app(&entries, Some("code-package"), None).is_err());
    }
}
