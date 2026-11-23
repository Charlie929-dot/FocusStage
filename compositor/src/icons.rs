//! Resolve desktop-entry icons using the same theme lookup and image loaders as GTK.
use std::ffi::{CStr, CString, OsStr, c_char, c_int, c_void};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

use libloading::Library;

use crate::desktop;

const ICON_SIZE: c_int = 48;
type Object = *mut c_void;

struct Gtk {
    _library: Library,
    new_theme: unsafe extern "C" fn() -> Object,
    set_theme: unsafe extern "C" fn(Object, *const c_char),
    append_path: unsafe extern "C" fn(Object, *const c_char),
    lookup: unsafe extern "C" fn(Object, *const c_char, c_int, c_int) -> Object,
    filename: unsafe extern "C" fn(Object) -> *const c_char,
    pixbuf: unsafe extern "C" fn(*const c_char, c_int, c_int, c_int, *mut Object) -> Object,
    save: unsafe extern "C" fn(
        Object,
        *mut *mut u8,
        *mut usize,
        *const c_char,
        *mut *mut c_char,
        *mut *mut c_char,
        *mut Object,
    ) -> c_int,
    unref: unsafe extern "C" fn(Object),
    free: unsafe extern "C" fn(Object),
}

impl Gtk {
    fn load() -> Option<Self> {
        // GTK's stand-alone theme lookup works without initializing a display. On Linux its
        // shared library also exposes the GdkPixbuf/GLib symbols from its dependencies.
        unsafe {
            let library = Library::new("libgtk-3.so.0").ok()?;
            Some(Self {
                new_theme: *library.get(b"gtk_icon_theme_new\0").ok()?,
                set_theme: *library.get(b"gtk_icon_theme_set_custom_theme\0").ok()?,
                append_path: *library.get(b"gtk_icon_theme_append_search_path\0").ok()?,
                lookup: *library.get(b"gtk_icon_theme_lookup_icon\0").ok()?,
                filename: *library.get(b"gtk_icon_info_get_filename\0").ok()?,
                pixbuf: *library.get(b"gdk_pixbuf_new_from_file_at_scale\0").ok()?,
                save: *library.get(b"gdk_pixbuf_save_to_bufferv\0").ok()?,
                unref: *library.get(b"g_object_unref\0").ok()?,
                free: *library.get(b"g_free\0").ok()?,
                _library: library,
            })
        }
    }
}

struct GtkObject<'a>(NonNull<c_void>, &'a Gtk);

impl Drop for GtkObject<'_> {
    fn drop(&mut self) {
        unsafe { (self.1.unref)(self.0.as_ptr()) }
    }
}

pub struct DesktopIcons {
    theme: Option<GtkObject<'static>>,
}

impl DesktopIcons {
    pub fn new(application_dirs: &[PathBuf]) -> Self {
        let theme_name = std::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "icon-theme"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| String::from_utf8(out.stdout).ok());
        Self::with_theme(
            application_dirs,
            theme_name.as_deref().map(|s| s.trim().trim_matches('\'')),
        )
    }

    fn with_theme(application_dirs: &[PathBuf], theme_name: Option<&str>) -> Self {
        static GTK: OnceLock<Option<Gtk>> = OnceLock::new();
        let theme = GTK.get_or_init(Gtk::load).as_ref().and_then(|gtk| unsafe {
            let object = NonNull::new((gtk.new_theme)())?;
            // GTK already supplies the XDG search path. Also look in icon roots beside app
            // entries from exported catalogs that aren't on the current session's data path.
            for dir in application_dirs
                .iter()
                .filter(|p| p.ends_with("applications"))
            {
                if let Some(root) = dir.parent() {
                    for name in ["icons", "pixmaps"] {
                        if let Ok(path) = CString::new(root.join(name).as_os_str().as_bytes()) {
                            (gtk.append_path)(object.as_ptr(), path.as_ptr());
                        }
                    }
                }
            }
            if let Ok(name) =
                CString::new(theme_name.filter(|s| !s.is_empty()).unwrap_or("hicolor"))
            {
                (gtk.set_theme)(object.as_ptr(), name.as_ptr());
            }
            Some(GtkObject(object, gtk))
        });
        Self { theme }
    }

    /// Native Linux path for a .desktop Icon value. Resolve symlinks before handing the path
    /// to Windows, whose WSL share may not follow Linux-only paths such as /snap/*/current.
    pub fn resolve(&self, icon: &str) -> Option<PathBuf> {
        let path = Path::new(icon);
        if path.is_absolute() {
            return path
                .is_file()
                .then(|| fs::canonicalize(path).ok())
                .flatten();
        }
        if icon.is_empty() {
            return None;
        }
        let theme = self.theme.as_ref()?;
        let name = CString::new(icon).ok()?;
        unsafe {
            let info = GtkObject(
                NonNull::new((theme.1.lookup)(
                    theme.0.as_ptr(),
                    name.as_ptr(),
                    ICON_SIZE,
                    0,
                ))?,
                theme.1,
            );
            let filename = (theme.1.filename)(info.0.as_ptr());
            if filename.is_null() {
                return None;
            }
            fs::canonicalize(OsStr::from_bytes(CStr::from_ptr(filename).to_bytes())).ok()
        }
    }

    pub fn png(&self, path: &Path) -> Option<Vec<u8>> {
        let gtk = self.theme.as_ref()?.1;
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        unsafe {
            let pixbuf = GtkObject(
                NonNull::new((gtk.pixbuf)(
                    path.as_ptr(),
                    ICON_SIZE,
                    ICON_SIZE,
                    1,
                    ptr::null_mut(),
                ))?,
                gtk,
            );
            let mut buffer = ptr::null_mut();
            let mut len = 0;
            if (gtk.save)(
                pixbuf.0.as_ptr(),
                &mut buffer,
                &mut len,
                c"png".as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ) == 0
            {
                return None;
            }
            let png = std::slice::from_raw_parts(buffer, len).to_vec();
            (gtk.free)(buffer.cast());
            Some(png)
        }
    }

    /// The Windows launcher and shortcut generator can open raster art directly. Rasterize
    /// other Linux formats once; later catalog refreshes can reuse the same PNG file.
    pub fn windows_path(&self, path: PathBuf) -> Option<PathBuf> {
        if path.extension().and_then(OsStr::to_str).is_some_and(|ext| {
            ["png", "ico", "bmp", "jpg", "jpeg"]
                .iter()
                .any(|format| ext.eq_ignore_ascii_case(format))
        }) {
            return Some(path);
        }
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
        let dir = cache.join("focusstage/icons");
        let metadata = fs::metadata(&path).ok()?;
        let mut key = DefaultHasher::new();
        (
            path.as_path(),
            metadata.len(),
            metadata.modified().ok()?,
            ICON_SIZE,
        )
            .hash(&mut key);
        let dest = dir.join(format!("{:016x}.png", key.finish()));
        if !dest.is_file() {
            let png = self.png(&path)?;
            fs::create_dir_all(&dir).ok()?;
            let mut temp = tempfile::NamedTempFile::new_in(&dir).ok()?;
            temp.write_all(&png).ok()?;
            temp.persist(&dest).ok()?;
        }
        Some(dest)
    }
}

pub(crate) fn app_icon_png(wm_class: &str) -> Option<String> {
    use base64::{Engine, engine::general_purpose::STANDARD};

    let dirs = desktop::launcher_application_dirs();
    let app = desktop::app_by_id(&dirs, wm_class)?;
    let icons = DesktopIcons::new(&dirs);
    let path = icons.resolve(app.icon.as_deref()?)?;
    Some(STANDARD.encode(icons.png(&path)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_inherited_theme_icons_and_rasterizes_svg() {
        let temp = tempfile::tempdir().unwrap();
        let apps = temp.path().join("applications");
        let icons = temp.path().join("icons");
        let setup_theme = |name: &str, inherits: &str| {
            let dir = icons.join(name);
            let sizes = dir.join("48x48/apps");
            fs::create_dir_all(&sizes).unwrap();
            fs::write(dir.join("index.theme"), format!(
                "[Icon Theme]\nName={name}\nInherits={inherits}\nDirectories=48x48/apps\n\n[48x48/apps]\nSize=48\nType=Fixed\nContext=Applications\n"
            )).unwrap();
            sizes
        };
        setup_theme("FocusStage-Test-Theme", "FocusStage-Test-Parent");
        let inherited = setup_theme("FocusStage-Test-Parent", "hicolor");
        let svg = inherited.join("com.focusstage.TestIcon.svg");
        fs::write(&svg, r##"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48"><rect width="48" height="48" fill="#2563eb"/></svg>"##).unwrap();
        let resolver = DesktopIcons::with_theme(&[apps], Some("FocusStage-Test-Theme"));
        if resolver.theme.is_none() {
            eprintln!("GTK 3 runtime not installed; skipping theme lookup");
            return;
        }
        let resolved = resolver.resolve("com.focusstage.TestIcon").unwrap();
        assert_eq!(resolved, fs::canonicalize(&svg).unwrap());
        assert_eq!(resolver.resolve(svg.to_str().unwrap()), Some(resolved));
        assert!(resolver.resolve("com.focusstage.Missing").is_none());
        let png = resolver.png(&svg).unwrap();
        let decoded = png::Decoder::new(std::io::Cursor::new(png))
            .read_info()
            .unwrap();
        assert_eq!((decoded.info().width, decoded.info().height), (48, 48));
    }
}
