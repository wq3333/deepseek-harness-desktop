use std::fs::File;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{Emitter, Manager};

/// Default port used by the DSH web server. Override at runtime with the
/// `DSH_PORT` environment variable (useful to test without touching 3080).
const DEFAULT_PORT: u16 = 3080;

/// How long (after the port opens) to wait for the `dsh web:` stdout line
/// that announces the per-process access token before giving up and falling
/// back to the persisted token / restarting the service.
const AUTH_LINE_TIMEOUT: Duration = Duration::from_secs(30);

/// Official DeepSeek Chat web app (requires network).
const CHAT_URL: &str = "https://chat.deepseek.com";

/// GitHub repo used by the 关于 (About) dialog for update checks and updates.
const GITHUB_REPO: &str = "wq3333/deepseek-harness-desktop";
/// GitHub API endpoint for the latest published release.
const GITHUB_LATEST_API: &str =
    "https://api.github.com/repos/wq3333/deepseek-harness-desktop/releases/latest";

/// Height (logical px) of the custom title bar.
const TITLE_BAR_HEIGHT: f64 = 44.0;

/// Window / harness-content background color in light theme — matches the
/// title bar and loading page so there is never a white flash before paint.
const LIGHT_BG: tauri::window::Color = tauri::window::Color(246, 248, 250, 255);
/// Same for dark theme (GitHub-dark content background).
const DARK_BG: tauri::window::Color = tauri::window::Color(13, 17, 23, 255);

/// Minimum logical window size when restoring a persisted geometry.
const MIN_WINDOW_W: f64 = 800.0;
const MIN_WINDOW_H: f64 = 600.0;

/// How long window move/resize events are debounced before writing the
/// geometry to disk (they fire many times per second while dragging).
const GEOMETRY_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// The saved window rectangle must overlap some monitor by at least this many
/// physical pixels on both axes to be considered reachable after restore
/// (otherwise the window would open off-screen with no grabbable title bar).
const GEOMETRY_VISIBLE_MARGIN: f64 = 48.0;

/// WebView2 Evergreen runtime bootstrapper download URL (used for the
/// automatic WebView2 installation in the native pre-flight phase).
const WEBVIEW2_BOOTSTRAPPER_URL: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";
/// Registry client GUID of the WebView2 runtime (Evergreen).
const WEBVIEW2_CLIENT_GUID: &str = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";

/// Holds the child process spawned by this instance (if any), so it can be
/// cleaned up on exit.
struct ServerState(Mutex<Option<Child>>);

/// Current height (logical px) of the title bar webview: 44 normally, taller
/// while the "更多" dropdown (full window) or a toast (small) is showing.
struct BarHeight(Mutex<f64>);

/// Which content webview is currently visible ("harness" or "chat"). Used by
/// the F12 shortcut to open DevTools on the page the user is actually viewing.
struct CurrentTarget(Mutex<String>);

/// Live per-target (dsh / desktop app) check & update state shared with the
/// merged 关于 panel of the title bar. Written by the check/update commands
/// (possibly from background threads) and broadcast via the `about-progress`
/// event; the panel also queries it through `get_about_state`, so closing and
/// reopening the popup keeps every status.
#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TargetState {
    /// idle | checking | downloading | installing | restarting | finalizing | done | error
    phase: String,
    /// 0..=100 when the progress is measurable; None = indeterminate bar.
    progress: Option<f64>,
    message: String,
    error: Option<String>,
    /// Installed / current version (dsh from npm, app from package info).
    current: Option<String>,
    /// Latest published version (dsh from the npm registry, app from GitHub).
    latest: Option<String>,
    update_available: bool,
    /// Desktop-only: release notes of the newest GitHub release.
    release_notes: String,
    /// Desktop-only: download URL of the newest portable exe.
    asset_url: String,
}

/// Latest check/update state for both targets, readable at any time via
/// `get_about_state` and broadcast via `about-progress`.
#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AboutState {
    dsh: TargetState,
    app: TargetState,
}

/// Shared per-target check/update state.
struct SharedAboutState(Mutex<AboutState>);

/// One row of the startup environment checklist shown on the loading page
/// (WebView2 / Node.js / dsh / dsh 服务).
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupItem {
    key: String,     // "webview2" | "node" | "dsh" | "service"
    label: String,   // display name
    status: String,  // pending | checking | ok | installing | installed | failed
    detail: String,  // version / failure reason
}

/// Snapshot of the startup environment check / auto-install progress, emitted
/// to the loading page via the `setup-progress` event and readable at any time
/// through `get_setup_state`.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupState {
    /// detecting | installing | starting | ready | error
    phase: String,
    message: String,
    /// 0..=100 for real byte downloads; None = indeterminate progress bar.
    progress: Option<f64>,
    items: Vec<SetupItem>,
}

/// Latest setup state, readable at any time via `get_setup_state`.
struct SharedSetupState(Mutex<SetupState>);

/// True while a setup/auto-install run is in flight (prevents a second run).
struct SetupRunning(Mutex<bool>);

// --- 设置 (user settings, persisted to settings.json) ---

/// User-adjustable settings persisted to `settings.json` in the app data dir.
/// - `close_stops_dsh`: stop the DSH server when the window closes (default off).
/// - `auto_update`: at startup check + update dsh and the desktop app (default on).
/// - `theme`: "light" | "dark" | "system" (follow the OS; default).
///
/// `#[serde(default)]` keeps old settings.json files (without `theme`) fully
/// readable, so upgrading never resets an existing user's preferences.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Settings {
    close_stops_dsh: bool,
    auto_update: bool,
    theme: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            close_stops_dsh: false,
            auto_update: true,
            theme: "system".into(),
        }
    }
}

/// Path of the persisted settings file (app data dir; falls back to the temp
/// dir when the platform dir cannot be resolved).
fn settings_path(app: &tauri::AppHandle) -> std::path::PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("settings.json")
}

/// Read a UTF-8 text file, tolerating a leading UTF-8 BOM (some editors and
/// PowerShell's `Set-Content -Encoding UTF8` add one, which would otherwise
/// break serde_json parsing of the whole file).
fn read_text_utf8(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

/// Read the persisted settings; any read/parse failure falls back to defaults
/// so a missing or corrupt file never breaks startup.
fn load_settings(app: &tauri::AppHandle) -> Settings {
    read_text_utf8(&settings_path(app))
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[tauri::command]
fn get_settings(app: tauri::AppHandle) -> Settings {
    load_settings(&app)
}

#[tauri::command]
fn save_settings(app: tauri::AppHandle, settings: Settings) -> Result<(), String> {
    let old = load_settings(&app);
    let path = settings_path(&app);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建设置目录失败:{e}"))?;
    }
    let json = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("写入设置失败:{e}"))?;
    // A theme change applies immediately to the window and every webview.
    if old.theme != settings.theme {
        apply_theme(&app);
    }
    Ok(())
}

// --- 主题 (theme: light / dark / follow system) ---

/// Map the persisted theme setting to a concrete light/dark value.
/// "system" follows the OS theme reported by Tauri (`Window::theme`).
fn resolve_theme(setting: &str, os: tauri::Theme) -> &'static str {
    match setting {
        "light" => "light",
        "dark" => "dark",
        _ => match os {
            tauri::Theme::Dark => "dark",
            _ => "light",
        },
    }
}

/// Background color matching a resolved theme value.
fn theme_bg(theme: &str) -> tauri::window::Color {
    if theme == "dark" {
        DARK_BG
    } else {
        LIGHT_BG
    }
}

/// JS snippet applying a resolved theme to the current document: it drives
/// `color-scheme` (so embedded pages matching `prefers-color-scheme` follow)
/// and sets `data-dsh-theme` on `<html>` (which the title bar and loading
/// page use in their CSS). Used both by runtime `eval` and baked into init
/// scripts.
fn theme_apply_js(theme: &str) -> String {
    let v = if theme == "dark" { "'dark'" } else { "'light'" };
    format!(
        "(function(t){{var d=document.documentElement;d.style.colorScheme=t;d.setAttribute('data-dsh-theme',t);}})({v})"
    )
}

/// Static init-script form of the theme (no closure), baked into webviews so
/// the very first paint of a page is already themed. The root element does not
/// exist yet when this runs (init scripts execute at document-start), so it
/// waits for `document.documentElement` via animation frames — which still
/// fire before the first paint — instead of throwing
/// "Cannot read properties of null (reading 'style')".
fn theme_init_script(theme: &str) -> String {
    let v = if theme == "dark" { "dark" } else { "light" };
    format!(
        "(function(){{function t(){{var d=document.documentElement;if(!d){{requestAnimationFrame(t);return;}}d.style.colorScheme='{v}';d.setAttribute('data-dsh-theme','{v}');}}t();}})();"
    )
}

/// Resolved theme for the app's main window right now (setting + OS theme).
fn current_theme(app: &tauri::AppHandle) -> &'static str {
    let setting = load_settings(app).theme;
    let os = app
        .get_window("main")
        .and_then(|w| w.theme().ok())
        .unwrap_or(tauri::Theme::Light);
    resolve_theme(&setting, os)
}

/// Re-apply the current theme to the window and all three webviews. Called on
/// a settings change and on OS theme changes while in "system" mode. The bar
/// webview is transparent (it stretches over the content), so only the window
/// and the two content webviews get an opaque background color.
fn apply_theme(app: &tauri::AppHandle) {
    let theme = current_theme(app);
    let bg = theme_bg(theme);
    if let Some(window) = app.get_window("main") {
        let _ = window.set_background_color(Some(bg));
    }
    for label in ["harness-content", "chat-content"] {
        if let Some(wv) = app.get_webview(label) {
            let _ = wv.set_background_color(Some(bg));
            let _ = wv.eval(theme_apply_js(theme));
        }
    }
    if let Some(bar) = app.get_webview("bar") {
        let _ = bar.eval(theme_apply_js(theme));
    }
}

/// Apply the current theme to a single webview (used by the page-load hook so
/// every finished navigation — loading -> dsh, chat first load — re-applies
/// the latest theme). `with_background` is false for the transparent bar.
fn apply_theme_to_webview(webview: &tauri::Webview, app: &tauri::AppHandle, with_background: bool) {
    let theme = current_theme(app);
    if with_background {
        let _ = webview.set_background_color(Some(theme_bg(theme)));
    }
    let _ = webview.eval(theme_apply_js(theme));
}

// --- 窗口状态 (window position / size, persisted to window-state.json) ---

/// Persisted window geometry (`window-state.json`), all values in logical
/// pixels. Written by Rust only, so saving user settings never touches it.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WindowState {
    maximized: bool,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Default for WindowState {
    fn default() -> Self {
        WindowState {
            maximized: false,
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        }
    }
}

/// Pending window geometry + time of the last disk write (debounced).
struct WindowStateCache {
    pending: Option<WindowState>,
    last_saved: Option<Instant>,
}

/// Shared window-geometry cache, managed by Tauri.
struct WindowStateStore(Mutex<WindowStateCache>);

fn window_state_path(app: &tauri::AppHandle) -> std::path::PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("window-state.json")
}

fn load_window_state(app: &tauri::AppHandle) -> Option<WindowState> {
    let text = read_text_utf8(&window_state_path(app))?;
    let state: WindowState = serde_json::from_str(&text).ok()?;
    Some(state)
}

fn save_window_state(app: &tauri::AppHandle, state: &WindowState) {
    let path = window_state_path(app);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, json);
    }
}

/// True when a window rectangle (logical coords, relative to the virtual
/// desktop origin, scaled by `scale`) overlaps at least one monitor rect
/// (physical) by `margin` px on both axes — i.e. the title bar stays
/// grabbable after restore. Monitors are `(x, y, width, height)`.
fn rect_visible_on_monitors(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    scale: f64,
    monitors: &[(i32, i32, u32, u32)],
    margin: f64,
) -> bool {
    let rx = x * scale;
    let ry = y * scale;
    let rw = w * scale;
    let rh = h * scale;
    monitors.iter().any(|&(mx, my, mw, mh)| {
        let ox = (rx + rw).min((mx + mw as i32) as f64) - rx.max(mx as f64);
        let oy = (ry + rh).min((my + mh as i32) as f64) - ry.max(my as f64);
        ox >= margin && oy >= margin
    })
}

/// Off-screen guard for restoring a saved position: false when the saved rect
/// would be unreachable on every connected monitor.
fn saved_geometry_visible(state: &WindowState, window: &tauri::Window) -> bool {
    let scale = window
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| m.scale_factor())
        .unwrap_or(1.0);
    let monitors: Vec<(i32, i32, u32, u32)> = window
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| {
            let p = m.position();
            let s = m.size();
            (p.x, p.y, s.width, s.height)
        })
        .collect();
    rect_visible_on_monitors(
        state.x,
        state.y,
        state.width,
        state.height,
        scale,
        &monitors,
        GEOMETRY_VISIBLE_MARGIN,
    )
}

/// Record the window's current geometry into the shared store, writing to
/// disk at most once per debounce interval. Minimized windows are skipped
/// (Windows reports a near-zero size for them); a maximized window only flips
/// the flag so the last known normal bounds survive for the unmaximize case.
fn record_window_geometry(window: &tauri::Window, app: &tauri::AppHandle) {
    if window.is_minimized().unwrap_or(true) {
        return;
    }
    let maximized = window.is_maximized().unwrap_or(false);
    let store = app.state::<WindowStateStore>();
    let mut cache = store.0.lock().unwrap();
    let state = cache.pending.get_or_insert_with(WindowState::default);
    state.maximized = maximized;
    if !maximized {
        if let (Ok(pos), Ok(size), Ok(scale)) =
            (window.outer_position(), window.outer_size(), window.scale_factor())
        {
            let scale = scale.max(0.1);
            state.x = pos.x as f64 / scale;
            state.y = pos.y as f64 / scale;
            state.width = size.width as f64 / scale;
            state.height = size.height as f64 / scale;
        }
    }
    if cache
        .last_saved
        .map_or(true, |t| t.elapsed() >= GEOMETRY_SAVE_DEBOUNCE)
    {
        if let Some(p) = &cache.pending {
            save_window_state(app, p);
        }
        cache.last_saved = Some(Instant::now());
    }
}

/// Final write of the pending geometry (called on every exit path). When the
/// window is minimized the last good pending state is kept as-is.
fn flush_window_state(app: &tauri::AppHandle) {
    if let Some(window) = app.get_window("main") {
        if !window.is_minimized().unwrap_or(true) {
            record_window_geometry(&window, app);
        }
    }
    let store = app.state::<WindowStateStore>();
    let cache = store.0.lock().unwrap();
    if let Some(p) = &cache.pending {
        save_window_state(app, p);
    }
}

/// Apply the persisted geometry to a (hidden, already built) window: normal
/// bounds when they are valid, else centered. First run (no state) stays
/// centered. Returns true when the saved state was maximized — the caller
/// defers the actual maximize until the first page has finished loading, so
/// the maximized window never appears empty (no startup flash).
fn restore_window_geometry(window: &tauri::Window, app: &tauri::AppHandle) -> bool {
    let Some(state) = load_window_state(app) else {
        let _ = window.center();
        return false;
    };
    let w = state.width.max(MIN_WINDOW_W);
    let h = state.height.max(MIN_WINDOW_H);
    let _ = window.set_size(tauri::LogicalSize::new(w, h));
    if saved_geometry_visible(&state, window) {
        let _ = window.set_position(tauri::LogicalPosition::new(state.x, state.y));
    } else {
        let _ = window.center();
    }
    state.maximized
}

fn default_setup_items() -> Vec<SetupItem> {
    vec![
        SetupItem { key: "webview2".into(), label: "WebView2 运行时".into(), status: "pending".into(), detail: String::new() },
        SetupItem { key: "node".into(), label: "Node.js".into(), status: "pending".into(), detail: String::new() },
        SetupItem { key: "dsh".into(), label: "dsh".into(), status: "pending".into(), detail: String::new() },
        SetupItem { key: "service".into(), label: "dsh 服务".into(), status: "pending".into(), detail: String::new() },
    ]
}

fn default_setup_state() -> SetupState {
    SetupState {
        phase: "detecting".into(),
        message: "正在检测运行环境…".into(),
        progress: None,
        items: default_setup_items(),
    }
}

/// Update one checklist row (status + detail) inside a SetupState copy.
fn set_setup_item(state: &mut SetupState, key: &str, status: &str, detail: impl Into<String>) {
    if let Some(item) = state.items.iter_mut().find(|i| i.key == key) {
        item.status = status.to_string();
        item.detail = detail.into();
    }
}

/// Persist + broadcast the setup state to the loading page.
fn publish_setup(app: &tauri::AppHandle, state: SetupState) {
    *app.state::<SharedSetupState>().0.lock().unwrap() = state.clone();
    let _ = app.emit("setup-progress", state);
}

/// Append one detail line to the loading page's live log (`setup-log` event).
fn setup_log(app: &tauri::AppHandle, line: impl Into<String>) {
    let _ = app.emit("setup-log", line.into());
}

/// True while a check/update is in flight for one target (prevents starting
/// another one on the same target).
fn target_active(state: &TargetState) -> bool {
    matches!(
        state.phase.as_str(),
        "checking" | "downloading" | "installing" | "restarting" | "finalizing"
    )
}

/// True while any check/update is in flight (guards the single 检查更新 button
/// of the 关于 panel and the startup auto-update).
fn about_active(app: &tauri::AppHandle) -> bool {
    let st = app.state::<SharedAboutState>().0.lock().unwrap().clone();
    target_active(&st.dsh) || target_active(&st.app)
}

/// Mutate one target of the shared AboutState ("dsh" | "app") and broadcast a
/// snapshot of the whole state to the title bar UI via `about-progress`.
fn update_target(app: &tauri::AppHandle, key: &str, f: impl FnOnce(&mut TargetState)) {
    let state = app.state::<SharedAboutState>();
    let mut st = state.0.lock().unwrap();
    let target = if key == "dsh" { &mut st.dsh } else { &mut st.app };
    f(target);
    let snapshot = st.clone();
    drop(st);
    let _ = app.emit("about-progress", snapshot);
}

/// Return the current check/update state for both targets (the 关于 panel
/// restores it when reopened after being closed mid-check/mid-update).
#[tauri::command]
fn get_about_state(app: tauri::AppHandle) -> AboutState {
    app.state::<SharedAboutState>().0.lock().unwrap().clone()
}

/// Spawn console-subsystem children (netstat, taskkill, npm, npx...) without
/// flashing a console window next to the app: this is a GUI process, so any
/// console child would otherwise pop a black box.
#[cfg(target_os = "windows")]
fn hidden(mut cmd: Command) -> Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}
#[cfg(not(target_os = "windows"))]
fn hidden(cmd: Command) -> Command {
    cmd
}

/// Run a program hidden (no console window) and return its stdout as UTF-8.
fn run_capture(program: &str, args: &[&str]) -> Result<String, String> {
    let output = hidden(Command::new(program))
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !err.is_empty() {
            return Err(err);
        }
        return Err(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run a program hidden and wait, returning its exit status (no output capture —
/// used for elevated installers like winget/msiexec where piped stdout can hang).
fn run_status(program: &str, args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    hidden(Command::new(program)).args(args).status()
}

/// Read machine + user PATH from the registry (what a freshly launched process
/// would see) and merge them into this process's PATH, so tools installed by
/// this app mid-run (Node.js, npm, the user-prefix dsh) are found by subsequent
/// children without a restart. Registry entries win; current entries are kept
/// (deduped) as a fallback for entries that are not registry-backed.
fn refresh_process_path() {
    let ps = "[Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')";
    let reg = run_ps(&ps).unwrap_or_default();
    if reg.trim().is_empty() {
        return;
    }
    let mut parts: Vec<String> = Vec::new();
    for p in reg.split(';') {
        let t = p.trim().to_string();
        if !t.is_empty() && !parts.contains(&t) {
            parts.push(t);
        }
    }
    if let Ok(cur) = std::env::var("Path") {
        for p in cur.split(';') {
            let t = p.trim().to_string();
            if !t.is_empty() && !parts.contains(&t) {
                parts.push(t);
            }
        }
    }
    std::env::set_var("Path", parts.join(";"));
}

/// Directory used for the no-admin fallback global npm prefix.
fn user_npm_prefix() -> std::path::PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(base).join("deepseek-harness").join("npm")
}

/// Append `dir` to the user PATH (HKCU\Environment) if not already present.
fn add_user_path(dir: String) {
    let ps = format!(
        "$p = [Environment]::GetEnvironmentVariable('Path','User')\r\n\
         if ($p -split ';' -contains '{dir}') {{ exit 0 }}\r\n\
         [Environment]::SetEnvironmentVariable('Path', ($p + ';' + '{dir}'), 'User')",
        dir = dir,
    );
    let _ = run_ps(&ps);
}

/// Check whether the WebView2 runtime is installed (Evergreen), via the
/// standard EdgeUpdate client registry keys (x64, x86, per-user) plus a
/// filesystem fallback for unusual (e.g. fixed-version) installs.
fn webview2_installed() -> bool {
    let guid = WEBVIEW2_CLIENT_GUID;
    for hive in [
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients",
        r"HKLM\SOFTWARE\Microsoft\EdgeUpdate\Clients",
        r"HKCU\SOFTWARE\Microsoft\EdgeUpdate\Clients",
    ] {
        let key = format!(r"{hive}\{guid}");
        if run_capture("reg", &["query", &key, "/v", "pv"]).is_ok() {
            return true;
        }
    }
    for d in [
        r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application",
        r"C:\Program Files\Microsoft\EdgeWebView\Application",
    ] {
        if std::fs::read_dir(d).map(|mut it| it.next().is_some()).unwrap_or(false) {
            return true;
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let d = std::path::Path::new(&local)
            .join("Microsoft")
            .join("EdgeWebView")
            .join("Application");
        if std::fs::read_dir(&d).map(|mut it| it.next().is_some()).unwrap_or(false) {
            return true;
        }
    }
    false
}

/// Native (no webview needed) blocking message box, used only for the WebView2
/// pre-flight phase before any page can exist.
#[cfg(target_os = "windows")]
fn native_msg(title: &str, text: &str, is_error: bool) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
    };
    let title: Vec<u16> = std::ffi::OsStr::new(title)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let text: Vec<u16> = std::ffi::OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let flags = MB_OK | if is_error { MB_ICONERROR } else { MB_ICONINFORMATION };
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), flags);
    }
}
#[cfg(not(target_os = "windows"))]
fn native_msg(_title: &str, _text: &str, _is_error: bool) {}

/// Download + install the WebView2 Evergreen runtime using the official
/// bootstrapper. Runs entirely in the native pre-flight phase (no webview
/// exists yet, so no page progress is possible here). The bootstrapper is
/// launched WITHOUT /silent so its own window shows the real download/install
/// progress; it may trigger a UAC prompt for the per-machine install.
fn install_webview2() -> Result<(), String> {
    let exe = std::env::temp_dir().join("MicrosoftEdgeWebview2Setup.exe");
    let _ = std::fs::remove_file(&exe);
    download_with_progress(WEBVIEW2_BOOTSTRAPPER_URL, &exe, &mut |_| {})?;
    let status = run_status(exe.to_str().unwrap_or_default(), &["/install"])
        .map_err(|e| format!("启动 WebView2 安装程序失败:{e}"))?;
    if !status.success() {
        return Err(format!("WebView2 安装程序退出码:{}", status.code().unwrap_or(-1)));
    }
    if webview2_installed() {
        Ok(())
    } else {
        Err("安装完成后仍未检测到 WebView2 运行时".into())
    }
}

/// Parse the installed version out of `npm ls -g @deepseek-ai/dsh --depth=0`
/// output (e.g. "`-- @deepseek-ai/dsh@0.1.0-rc.7").
fn parse_npm_version(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let at = line.rfind("@deepseek-ai/dsh@")?;
        let rest = &line[at + "@deepseek-ai/dsh@".len()..];
        let v = rest.split_whitespace().next().unwrap_or("");
        if v.is_empty() {
            None
        } else {
            Some(v.to_string())
        }
    })
}

fn active_port() -> u16 {
    std::env::var("DSH_PORT")
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT)
}

fn server_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn port_open(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
}

fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if port_open(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// Temp log file capturing the dsh server process stdout/stderr (per port), so
/// a failed start can be diagnosed from the UI instead of an eternal spinner.
fn server_log_path(port: u16) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("dsh-server-{port}.log"))
}

/// Last `n` non-empty lines of the per-port server log.
fn log_tail(port: u16, n: usize) -> String {
    let text = read_file(&server_log_path(port));
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// --- dsh web 访问令牌 (per-process browser-auth token) ---

/// Parse the authenticated dsh web URL out of the server's output.
///
/// dsh prints `dsh web: http://127.0.0.1:<port>/?token=...` to stdout after
/// the loader settles (also with `--no-open`, and older versions print a
/// plain URL without a token). The parser only keys on the `dsh web:` marker
/// and takes the first `http://` substring that follows, cutting at
/// whitespace, `(` (the optional ` (LAN: ...)` suffix) or an ANSI escape, so
/// it stays robust against colors, LAN suffixes and npx noise.
fn parse_auth_url(text: &str) -> Option<String> {
    let marker = "dsh web:";
    for line in text.lines() {
        let Some(marker_at) = line.find(marker) else {
            continue;
        };
        let Some(rel) = line[marker_at..].find("http://") else {
            continue;
        };
        let start = marker_at + rel;
        let rest = &line[start..];
        let end = rest
            .find(|c| c == ' ' || c == '(' || c == '\u{1b}')
            .unwrap_or(rest.len());
        let url = rest[..end].trim();
        if !url.is_empty() {
            return Some(url.to_string());
        }
    }
    None
}

/// Extract the current server's authenticated URL from the per-port log
/// (None when the log is missing or dsh has not announced its URL yet).
fn auth_url_from_log(port: u16) -> Option<String> {
    parse_auth_url(&read_file(&server_log_path(port)))
}

/// Wait for the `dsh web:` URL line to appear in the log (used right after
/// (re)starting the server: the port opens slightly before the URL line is
/// printed). Always reads once first, so a zero timeout is a single try.
fn wait_auth_url(port: u16, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(url) = auth_url_from_log(port) {
            return Some(url);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Extract the `token` query parameter from an authenticated dsh web URL.
fn url_token(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
}

/// Check that a previously captured token still authenticates the server on
/// `port`. The token belongs to one server process, so a stale token (server
/// restarted elsewhere) returns false. URLs without a token (legacy dsh)
/// cannot be probed and are treated as valid.
fn probe_auth_url(port: u16, url: &str) -> bool {
    let Some(token) = url_token(url) else {
        return true;
    };
    let mut stream = match std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(3),
    ) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let req = format!(
        "GET /?token={token} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 32];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    head.starts_with("HTTP/1.1 303") || head.starts_with("HTTP/1.1 200")
}

/// Path of the persisted last-known authenticated URL (app data dir), so a
/// still-running server from an earlier app run can be reconnected even if
/// the temp server log has been cleaned up.
fn auth_url_file(app: &tauri::AppHandle) -> std::path::PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("auth-url.txt")
}

fn persisted_auth_url(app: &tauri::AppHandle) -> Option<String> {
    let text = std::fs::read_to_string(auth_url_file(app)).ok()?;
    let url = text.trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

fn save_auth_url(app: &tauri::AppHandle, url: &str) {
    let file = auth_url_file(app);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, url);
}

/// Resolve the URL the harness webview should be pointed at, once the server
/// is known to be up on `port`:
/// 1. the just-started (or previously-app-started) server's log line;
/// 2. the persisted URL from an earlier run (same still-running process);
/// 3. failing both, restart the service so we own the process and can read
///    its token from stdout;
/// 4. legacy dsh (no token line) falls back to the plain server URL.
/// Returns None only when a restart attempt itself failed (the error is
/// already published to the loading page by `wait_server_ready`).
fn ensure_auth_url(app: &tauri::AppHandle, port: u16) -> Option<String> {
    let spawned_by_us = app.state::<ServerState>().0.lock().unwrap().is_some();
    let timeout = if spawned_by_us {
        AUTH_LINE_TIMEOUT
    } else {
        Duration::ZERO
    };

    // 1) token announced by the running server (fresh spawn or previous run).
    if let Some(url) = wait_auth_url(port, timeout) {
        if probe_auth_url(port, &url) {
            save_auth_url(app, &url);
            return Some(url);
        }
    }

    // 2) persisted URL for a still-running server we started earlier.
    if let Some(url) = persisted_auth_url(app) {
        if probe_auth_url(port, &url) {
            return Some(url);
        }
    }

    // 3) no usable token: restart so we own the process and capture its line.
    setup_log(app, "  未获取到 dsh 访问令牌，正在重启 dsh 服务以建立连接…");
    let mut s = app.state::<SharedSetupState>().0.lock().unwrap().clone();
    s.phase = "starting".into();
    s.message = "正在重启 dsh 服务以获取访问令牌…".into();
    s.progress = None;
    publish_setup(app, s);
    stop_dsh(app, port);
    match spawn_server(port) {
        Ok(child) => {
            *app.state::<ServerState>().0.lock().unwrap() = Some(child);
        }
        Err(e) => {
            setup_log(app, format!("  重启 dsh 服务失败:{e}"));
            return None;
        }
    }
    if !wait_server_ready(app, port, Duration::from_secs(60)) {
        return None; // wait_server_ready already published the error state
    }
    if let Some(url) = wait_auth_url(port, AUTH_LINE_TIMEOUT) {
        save_auth_url(app, &url);
        return Some(url);
    }
    // 4) legacy dsh: no token line printed, navigate plain.
    Some(server_url(port))
}

/// Wait for the dsh server to become ready on `port` while the loading page
/// shows live progress. Unlike `wait_for_port`, this fails fast: if the spawned
/// server child has already exited before the port opens, the port will never
/// open, so it reports the captured output immediately instead of spinning for
/// the full timeout. It also publishes a "已等待 Xs" status every few seconds.
/// Returns true when the port is ready.
fn wait_server_ready(app: &tauri::AppHandle, port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let started = Instant::now();
    let mut last_update = Instant::now();
    loop {
        if port_open(port) {
            return true;
        }
        // Fail fast: if the child we spawned already exited, the port will
        // never open — report the reason now, not after a long spin.
        let exited: Option<i32> = {
            let st = app.state::<ServerState>();
            let mut guard = st.0.lock().unwrap();
            match guard.as_mut().and_then(|c| c.try_wait().ok()).flatten() {
                Some(status) => Some(status.code().unwrap_or(-1)),
                None => None,
            }
        };
        if let Some(code) = exited {
            let tail = log_tail(port, 10);
            let reason = if tail.is_empty() {
                format!("dsh 服务进程提前退出(退出码 {code})")
            } else {
                format!("dsh 服务进程提前退出(退出码 {code})，最近输出：\n{tail}")
            };
            setup_log(app, format!("  {reason}"));
            let mut s = app.state::<SharedSetupState>().0.lock().unwrap().clone();
            s.phase = "error".into();
            s.message = format!("dsh 服务启动失败:{reason}");
            s.progress = None;
            publish_setup(app, s);
            return false;
        }
        if Instant::now() >= deadline {
            let tail = log_tail(port, 10);
            let msg = if tail.is_empty() {
                format!("等待 dsh 服务启动超时(端口 {port})")
            } else {
                format!("等待 dsh 服务启动超时(端口 {port})，最近输出：\n{tail}")
            };
            setup_log(app, format!("  {msg}"));
            return false;
        }
        if last_update.elapsed() >= Duration::from_secs(5) {
            last_update = Instant::now();
            let waited = started.elapsed().as_secs();
            let mut s = app.state::<SharedSetupState>().0.lock().unwrap().clone();
            s.message = format!("正在启动 dsh 服务…（已等待 {waited}s）");
            publish_setup(app, s);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn spawn_server(port: u16) -> std::io::Result<Child> {
    #[cfg(target_os = "windows")]
    {
        // CREATE_NO_WINDOW so no console window flashes next to the app.
        use std::os::windows::process::CommandExt;
        // Capture the child's stdout/stderr into the per-port log: that is the
        // only place the `dsh web: http://127.0.0.1:<port>/?token=...` line
        // (carrying the per-process access token) is announced, and it also
        // makes failed starts show real output. Truncating discards any stale
        // line from a previous server process on the same port.
        let log = File::create(server_log_path(port))?;
        let err = log.try_clone()?;
        Command::new("cmd.exe")
            .args([
                "/c",
                "npx",
                "@deepseek-ai/dsh",
                "web",
                "--no-open",
                "--port",
                &port.to_string(),
            ])
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err))
            .creation_flags(0x0800_0000)
            .spawn()
    }
}

/// Kill every process that is LISTENING on `port` (the server), killing its
/// process tree. Clients merely holding ESTABLISHED connections to the port
/// are intentionally left alone.
fn kill_port_owners(port: u16) {
    #[cfg(target_os = "windows")]
    {
        let Ok(output) = hidden(Command::new("netstat"))
            .args(["-ano", "-p", "tcp"])
            .output()
        else {
            return;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let needle = format!(":{port}");
        let mut pids: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for line in text.lines() {
            if line.contains(&needle) && line.contains("LISTENING") {
                if let Some(pid_str) = line.split_whitespace().last() {
                    if let Ok(pid) = pid_str.parse::<u32>() {
                        if pid != 0 {
                            pids.insert(pid);
                        }
                    }
                }
            }
        }
        for pid in pids {
            let _ = hidden(Command::new("taskkill"))
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .status();
        }
    }
}

fn kill_child_tree(child: &mut Child) {
    #[cfg(target_os = "windows")]
    {
        let _ = hidden(Command::new("taskkill"))
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .status();
        let _ = child.kill();
        let _ = child.wait();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Keep the child webviews of the single window laid out correctly:
/// title bar pinned to the top (height from `bar_height`, clamped to the
/// window), both content webviews (harness / chat) sharing the area below it
/// (mutually exclusive).
fn relayout(
    window: &tauri::Window,
    bar: &tauri::Webview,
    harness: &tauri::Webview,
    chat: &tauri::Webview,
    bar_height: f64,
) {
    let Ok(inner) = window.inner_size() else {
        return;
    };
    let scale = window.scale_factor().unwrap_or(1.0);
    let ls = inner.to_logical::<f64>(scale);
    // While the window is minimized Windows reports a tiny height (~19px);
    // `f64::clamp` panics when min > max, so never let the upper bound drop
    // below TITLE_BAR_HEIGHT (this is also safe when ls.height is NaN, since
    // f64::max returns the non-NaN operand).
    let bar_h = bar_height.clamp(TITLE_BAR_HEIGHT, ls.height.max(TITLE_BAR_HEIGHT));
    let _ = bar.set_position(tauri::LogicalPosition::new(0.0, 0.0));
    let _ = bar.set_size(tauri::LogicalSize::new(ls.width, bar_h));
    let content_h = (ls.height - TITLE_BAR_HEIGHT).max(0.0);
    let _ = harness.set_position(tauri::LogicalPosition::new(0.0, TITLE_BAR_HEIGHT));
    let _ = harness.set_size(tauri::LogicalSize::new(ls.width, content_h));
    let _ = chat.set_position(tauri::LogicalPosition::new(0.0, TITLE_BAR_HEIGHT));
    let _ = chat.set_size(tauri::LogicalSize::new(ls.width, content_h));
}

/// Re-layout on window resize, and turn any window close (title bar X,
/// Alt+F4) into a full app exit (without stopping the DSH server).
fn attach_window_handlers(
    window: tauri::Window,
    bar: tauri::Webview,
    harness: tauri::Webview,
    chat: tauri::Webview,
    app_handle: tauri::AppHandle,
) {
    let window_for_layout = window.clone();
    let app_handle_for_layout = app_handle.clone();
    window.on_window_event(move |event| {
        match event {
            tauri::WindowEvent::Resized(_) => {
                // Persist the geometry (skips minimized / maximized-size races
                // internally), then re-layout the webviews.
                record_window_geometry(&window_for_layout, &app_handle_for_layout);
                // Skip while minimized: Windows reports a ~19px height for the
                // minimized window and there is nothing to lay out until restore
                // (also avoids resizing webviews to a near-zero height).
                if window_for_layout.is_minimized().unwrap_or(true) {
                    return;
                }
                let bar_height = app_handle_for_layout
                    .state::<BarHeight>()
                    .0
                    .lock()
                    .unwrap()
                    .clone();
                relayout(&window_for_layout, &bar, &harness, &chat, bar_height);
            }
            tauri::WindowEvent::Moved(_) => {
                record_window_geometry(&window_for_layout, &app_handle_for_layout);
            }
            tauri::WindowEvent::ThemeChanged(_) => {
                // Only "system" mode follows live OS theme changes.
                if load_settings(&app_handle_for_layout).theme == "system" {
                    apply_theme(&app_handle_for_layout);
                }
            }
            _ => {}
        }
    });
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            quit_app(&app_handle);
        }
    });
}

/// Create the three child webviews of the single window: the persistent
/// title bar on top, and the two content webviews (harness / chat) that
/// toggle visibility below it. chat-content is added first and hidden right
/// away (it preloads in the background under harness-content).
///
/// `maximize_on_first_paint`: when the saved window state was maximized, the
/// maximize is deferred until the harness webview finishes loading its first
/// page (the loading page), so the maximized window always has real content —
/// no flash of an empty maximized window at startup.
fn add_webviews(
    window: tauri::Window,
    app_handle: tauri::AppHandle,
    maximize_on_first_paint: bool,
) -> tauri::Result<()> {
    // Resolve the theme before any webview exists: it is baked into each
    // webview's initialization script and background color, so the very first
    // paint of every page (title bar, loading, dsh, chat) is already themed —
    // no light->dark flash, and embedded pages that match `prefers-color-scheme`
    // (driven by the injected `color-scheme`) follow immediately.
    let theme = current_theme(&app_handle);
    let bg = theme_bg(theme);
    let init = theme_init_script(theme);
    let _ = window.set_background_color(Some(bg));

    // Re-apply the current theme on every finished navigation, so pages that
    // load later (chat on first switch, a theme change made while a webview
    // was hidden) still get the latest theme.
    let page_load_app = app_handle.clone();
    let page_load_apply = move |webview: tauri::Webview, payload: tauri::webview::PageLoadPayload<'_>| {
        if payload.event() == tauri::webview::PageLoadEvent::Finished {
            apply_theme_to_webview(&webview, &page_load_app, true);
        }
    };

    // One-shot guard: the harness's first finished page-load triggers the
    // deferred startup maximize (only once — later navigations must not
    // re-maximize).
    let maximize_first =
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(maximize_on_first_paint));
    let harness_maximize = maximize_first.clone();
    let harness_maximize_window = window.clone();
    let harness_page_load_app = app_handle.clone();
    let harness_page_load = move |webview: tauri::Webview, payload: tauri::webview::PageLoadPayload<'_>| {
        if payload.event() == tauri::webview::PageLoadEvent::Finished {
            apply_theme_to_webview(&webview, &harness_page_load_app, true);
            // Maximize only once the first visible page has loaded, so the
            // window never pops in maximized-but-empty.
            if harness_maximize.swap(false, std::sync::atomic::Ordering::SeqCst) {
                let _ = harness_maximize_window.maximize();
            }
        }
    };

    // Size the webviews from the window's actual logical size instead of a
    // hard-coded 1280x800: the loading page is then centered at the final
    // size from its very first paint, avoiding a one-time "jump" when the
    // (maximized) window gets relaid out at startup.
    let scale = window.scale_factor().unwrap_or(1.0);
    let inner = window
        .inner_size()
        .unwrap_or(tauri::PhysicalSize::new(1280, 800));
    let ls = inner.to_logical::<f64>(scale);
    let width = ls.width.max(1.0);
    let content_h = (ls.height - TITLE_BAR_HEIGHT).max(0.0);

    let chat = window.add_child(
        tauri::webview::WebviewBuilder::new(
            "chat-content",
            tauri::WebviewUrl::External(url::Url::parse(CHAT_URL).unwrap()),
        )
        .initialization_script(init.clone())
        .on_page_load(page_load_apply.clone())
        // <a target="_blank"> / window.open() links: open in the system
        // browser instead of being silently dropped (wry cancels every new
        // window request when no handler is registered).
        .on_new_window(move |url, _features| {
            if matches!(url.scheme(), "http" | "https") {
                open_in_browser(url.as_str());
            }
            tauri::webview::NewWindowResponse::Deny
        }),
        tauri::LogicalPosition::new(0.0, TITLE_BAR_HEIGHT),
        tauri::LogicalSize::new(width, content_h),
    )?;
    let _ = chat.hide();

    let harness = window.add_child(
        tauri::webview::WebviewBuilder::new(
            "harness-content",
            tauri::WebviewUrl::App("loading.html".into()),
        )
        .initialization_script(init.clone())
        .on_page_load(harness_page_load)
        // Match the loading page background so the webview never flashes white
        // before its first paint or during the loading -> dsh navigation
        // (WebView2 would otherwise show white). The window itself carries the
        // same background_color, so the pre-paint gap is seamless too.
        .background_color(bg)
        // External links open in the system browser; only the dsh server
        // origin and the app's own assets are allowed to navigate in-webview,
        // so clicking a link never kicks the user out of the Harness GUI.
        .on_navigation({
            let port = active_port();
            move |url| {
                if is_internal_url(url, port) {
                    true
                } else {
                    open_in_browser(url.as_str());
                    false
                }
            }
        })
        // <a target="_blank"> / window.open() links: same treatment as plain
        // navigation — open in the browser, never create a new webview window.
        .on_new_window(move |url, _features| {
            if matches!(url.scheme(), "http" | "https") {
                open_in_browser(url.as_str());
            }
            tauri::webview::NewWindowResponse::Deny
        }),
        tauri::LogicalPosition::new(0.0, TITLE_BAR_HEIGHT),
        tauri::LogicalSize::new(width, content_h),
    )?;

    // Persistent title bar; added last so it is topmost in z-order.
    // Transparent so that when the "更多" dropdown expands it to the full
    // window height, the content webviews beneath remain visible (the title
    // bar strip itself keeps its own opaque background).
    let bar_page_load_app = app_handle.clone();
    let bar = window.add_child(
        tauri::webview::WebviewBuilder::new("bar", tauri::WebviewUrl::App("index.html".into()))
            .initialization_script("window.__DSH_TARGET__ = 'harness';")
            .initialization_script(init)
            // The bar is transparent: only eval the theme, never paint a
            // background over the content beneath.
            .on_page_load(move |webview, payload| {
                if payload.event() == tauri::webview::PageLoadEvent::Finished {
                    apply_theme_to_webview(&webview, &bar_page_load_app, false);
                }
            })
            .transparent(true),
        tauri::LogicalPosition::new(0.0, 0.0),
        tauri::LogicalSize::new(width, TITLE_BAR_HEIGHT),
    )?;

    relayout(&window, &bar, &harness, &chat, TITLE_BAR_HEIGHT);
    attach_window_handlers(window, bar, harness, chat, app_handle);
    Ok(())
}

/// Resize the title bar webview to a given height. `height <= 0` means the
/// full window height (used by the settings popup); otherwise it is clamped
/// to `[TITLE_BAR_HEIGHT, window height]` (a small height is used for toasts).
/// When the webview shrinks (popup/toast closing), it is hidden for the
/// resize and re-shown at the final size, so the compositor never paints the
/// shrinking transparent full-window surface (the source of the close hitch).
#[tauri::command]
fn set_bar_height(app: tauri::AppHandle, height: f64) {
    let Some(bar) = app.get_webview("bar") else {
        return;
    };
    let Some(window) = app.get_window("main") else {
        return;
    };
    let Ok(inner) = window.inner_size() else {
        return;
    };
    let scale = window.scale_factor().unwrap_or(1.0);
    let ls = inner.to_logical::<f64>(scale);
    // Same guard as relayout(): a minimized/tiny window reports a height below
    // TITLE_BAR_HEIGHT, which would make f64::clamp panic (min > max).
    let max_h = ls.height.max(TITLE_BAR_HEIGHT);
    let h = if height <= 0.0 {
        max_h
    } else {
        height.clamp(TITLE_BAR_HEIGHT, max_h)
    };
    let bar_height_state = app.state::<BarHeight>();
    let mut bar_height = bar_height_state.0.lock().unwrap();
    let shrinking = h < *bar_height;
    *bar_height = h;
    drop(bar_height);
    // Shrinking the transparent full-window webview back to the title strip
    // makes WebView2 recomposite the whole surface, which can visibly hitch
    // right after the settings popup fades out. Resize while hidden and show
    // again at the final size in the same synchronous command, so the shrink
    // is never painted.
    if shrinking {
        let _ = bar.hide();
    }
    let _ = bar.set_position(tauri::LogicalPosition::new(0.0, 0.0));
    let _ = bar.set_size(tauri::LogicalSize::new(ls.width, h));
    if shrinking {
        let _ = bar.show();
    }
}

/// Switch which content webview is visible (harness dsh GUI vs chat web).
/// The single window and its persistent title bar never hide/show, so the
/// switch only toggles the two content webviews (show target, then hide the
/// other to avoid a blank frame).
#[tauri::command]
fn switch_to(app: tauri::AppHandle, target: String) {
    let harness = app.get_webview("harness-content");
    let chat = app.get_webview("chat-content");
    match target.as_str() {
        "harness" => {
            if let Some(w) = &harness {
                let _ = w.show();
            }
            if let Some(w) = &chat {
                let _ = w.hide();
            }
        }
        "chat" => {
            if let Some(w) = &chat {
                let _ = w.show();
            }
            if let Some(w) = &harness {
                let _ = w.hide();
            }
        }
        _ => return,
    }
    *app.state::<CurrentTarget>().0.lock().unwrap() = target.clone();
    // Keep the single title bar's active-button highlight in sync.
    if let Some(bar) = app.get_webview("bar") {
        let _ = bar.eval(&format!("setActive({target:?})"));
    }
}

/// Open/close the DevTools window of the content webview that is currently
/// visible (harness dsh GUI or chat web). Bound to the global F12 shortcut.
fn toggle_devtools(app: tauri::AppHandle) {
    let current = app.state::<CurrentTarget>().0.lock().unwrap().clone();
    let label = match current.as_str() {
        "chat" => "chat-content",
        _ => "harness-content",
    };
    if let Some(wv) = app.get_webview(label) {
        if wv.is_devtools_open() {
            let _ = wv.close_devtools();
        } else {
            let _ = wv.open_devtools();
        }
    }
}

/// Exit the whole app. When the "关闭窗口时关闭 dsh 服务" setting is enabled,
/// also stop the DSH server first (used by the title bar X, Alt+F4 and quit).
fn quit_app(app: &tauri::AppHandle) {
    flush_window_state(app);
    if load_settings(app).close_stops_dsh {
        stop_dsh(app, active_port());
    }
    app.exit(0);
}

/// Exit the whole app (title bar X button). The DSH server is stopped before
/// exiting only when the "关闭窗口时关闭 dsh 服务" setting is enabled; otherwise
/// the spawned DSH server keeps running (orphaned) so the port keeps serving.
#[tauri::command]
fn quit(app: tauri::AppHandle) {
    quit_app(&app);
}

/// Stop the DSH server: kill whatever is LISTENING on `port`, plus the child
/// process tree we spawned.
fn stop_dsh(app: &tauri::AppHandle, port: u16) {
    kill_port_owners(port);
    let state = app.state::<ServerState>();
    let mut guard = state.0.lock().unwrap();
    if let Some(mut child) = guard.take() {
        kill_child_tree(&mut child);
    }
}

/// Stop the DSH server, then quit the app (更多菜单 -> "关闭 dsh 并退出").
#[tauri::command]
fn quit_with_dsh(app: tauri::AppHandle) {
    flush_window_state(&app);
    let port = active_port();
    stop_dsh(&app, port);
    app.exit(0);
}

/// Restart the DSH server and refresh the harness view
/// ("更多" -> "重启 dsh 服务并刷新").
#[tauri::command]
fn restart_dsh(app: tauri::AppHandle) {
    let port = active_port();
    show_toast(&app, "正在重启dsh服务...");
    std::thread::spawn(move || {
        stop_dsh(&app, port);
        match spawn_server(port) {
            Ok(child) => {
                *app.state::<ServerState>().0.lock().unwrap() = Some(child);
                show_toast(&app, "dsh服务已重启");
            }
            Err(e) => {
                show_toast(&app, format!("failed to respawn dsh web server: {e}"));
                return;
            }
        }
        if wait_server_ready(&app, port, Duration::from_secs(60)) {
            // A fresh server process has a fresh access token: resolve it from
            // stdout and navigate with it, otherwise the new dsh returns 401.
            match ensure_auth_url(&app, port) {
                Some(u) => {
                    if let Ok(parsed) = url::Url::parse(&u) {
                        if let Some(content) = app.get_webview("harness-content") {
                            let _ = content.navigate(parsed);
                        }
                    }
                }
                None => show_toast(&app, "dsh 服务未能启动，请稍后重试"),
            }
        } else {
            let tail = log_tail(port, 6);
            if tail.is_empty() {
                show_toast(&app, "dsh 服务未能启动，请稍后重试");
            } else {
                show_toast(&app, format!("dsh 服务未能启动，最近输出：{}", tail));
            }
        }
    });
}

/// Show a transient toast in the title bar overlay (top-right, auto-dismiss).
/// Safe to call from any thread: it just evals into the bar webview.
fn show_toast(app: &tauri::AppHandle, message: impl Into<String>) {
    if let Some(bar) = app.get_webview("bar") {
        let _ = bar.eval(&format!("showToast({:?})", message.into()));
    }
}

/// Stop the DSH server but keep the window open (更多 -> "关闭 dsh"). The
/// harness view is pointed at a neutral "stopped" page so it doesn't show a
/// broken connection error.
#[tauri::command(rename = "stop_dsh")]
fn stop_dsh_cmd(app: tauri::AppHandle) {
    let port = active_port();
    stop_dsh(&app, port);
    if let Some(content) = app.get_webview("harness-content") {
        let _ = content
            .navigate(url::Url::parse("http://tauri.localhost/loading.html?mode=stopped").unwrap());
    }
    show_toast(&app, "dsh 服务已停止");
}

/// Spawn a PowerShell script file as a hidden background process, redirecting
/// its stdout/stderr into `log`. The returned handle lets the caller poll the
/// process while inspecting a partial output file for download progress.
fn spawn_ps_hidden(script_file: &std::path::Path, log: &std::path::Path) -> std::io::Result<Child> {
    let out = File::create(log)?;
    let err = out.try_clone()?;
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                script_file.to_str().unwrap_or_default(),
            ])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                script_file.to_str().unwrap_or_default(),
            ])
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
    }
}

/// Best-effort Content-Length of a URL (HEAD via PowerShell), used to compute
/// real download progress. Returns None when the header is unavailable.
fn head_content_length(url: &str) -> Option<u64> {
    let ps = format!(
        "$ProgressPreference='SilentlyContinue'\r\n\
         try {{\r\n\
         \x20 $r = Invoke-WebRequest -UseBasicParsing -Method Head -MaximumRedirection 10 -Headers @{{ 'User-Agent'='deepseek-harness-desktop ({GITHUB_REPO})' }} '{url}'\r\n\
         \x20 $r.Headers['Content-Length']\r\n\
         }} catch {{}}",
        url = url,
    );
    run_ps(&ps).ok()?.trim().parse::<u64>().ok()
}

fn read_file(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Download `url` to `dest` as a hidden PowerShell background process, polling
/// the partial file size for real percent progress (`on_progress(Some(pct))`)
/// when a Content-Length is available, else `on_progress(None)` for an
/// indeterminate bar. Used by the auto-install flow (Node.js, WebView2).
fn download_with_progress(
    url: &str,
    dest: &std::path::Path,
    on_progress: &mut dyn FnMut(Option<f64>),
) -> Result<(), String> {
    let total = head_content_length(url);
    let _ = std::fs::remove_file(dest);
    let log = std::env::temp_dir().join(format!("dsh-download-{}.error.log", std::process::id()));
    let ps_file = std::env::temp_dir().join(format!("dsh-download-{}.ps1", std::process::id()));
    let ps = format!(
        "$ProgressPreference='SilentlyContinue'\r\n\
         try {{\r\n\
         \x20 Invoke-WebRequest -UseBasicParsing -Headers @{{ 'User-Agent'='deepseek-harness-desktop ({GITHUB_REPO})' }} -OutFile '{dest}' '{url}'\r\n\
         \x20 exit 0\r\n\
         }} catch {{\r\n\
         \x20 $_.Exception.Message | Out-File -FilePath '{err}' -Encoding utf8\r\n\
         \x20 exit 1\r\n\
         }}",
        dest = dest.display(),
        url = url,
        err = log.display(),
    );
    std::fs::write(&ps_file, &ps).map_err(|e| format!("写入下载脚本失败:{e}"))?;
    let mut child = spawn_ps_hidden(&ps_file, &log).map_err(|e| format!("启动下载失败:{e}"))?;

    let mut last_pct = -1.0f64;
    let mut last_indeterminate = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = std::fs::remove_file(&ps_file);
                if !status.success() {
                    let msg = read_file(&log);
                    let _ = std::fs::remove_file(&log);
                    return Err(if msg.trim().is_empty() {
                        "下载失败".into()
                    } else {
                        msg.trim().to_string()
                    });
                }
                break;
            }
            Ok(None) => {
                let len = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
                match total.filter(|t| *t > 0) {
                    Some(total) => {
                        let pct = (len as f64 / total as f64 * 100.0).min(100.0);
                        if (pct - last_pct).abs() >= 1.0 {
                            last_pct = pct;
                            on_progress(Some(pct));
                        }
                    }
                    None => {
                        // No total size: throttle the indeterminate callback so
                        // the event stream stays light.
                        if last_indeterminate.elapsed() >= Duration::from_secs(1) {
                            last_indeterminate = std::time::Instant::now();
                            on_progress(None);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => {
                let _ = std::fs::remove_file(&ps_file);
                return Err("无法读取下载进程状态".into());
            }
        }
    }

    if std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err("下载结果为空".into());
    }
    Ok(())
}

/// Apply a dsh update: install the latest `@deepseek-ai/dsh` from npm, restart
/// the dsh service and navigate the harness view back to it
/// (关于 -> dsh 卡片 -> "更新 dsh"). All npm steps run hidden, in a background
/// thread, and progress is published to the dsh target of the 关于 panel.
#[tauri::command]
fn update_dsh(app: tauri::AppHandle) {
    if *app.state::<SetupRunning>().0.lock().unwrap() {
        show_toast(&app, "环境检测/安装正在进行中，请稍后再试");
        return;
    }
    {
        let st = app.state::<SharedAboutState>().0.lock().unwrap().clone();
        if target_active(&st.dsh) {
            show_toast(&app, "dsh 更新正在进行");
            return;
        }
    }
    update_target(&app, "dsh", |t| {
        t.phase = "checking".into();
        t.message = "正在准备更新 dsh…".into();
        t.progress = None;
        t.error = None;
    });
    let port = active_port();
    std::thread::spawn(move || match update_dsh_inner(&app, port) {
        Ok(msg) => {
            update_target(&app, "dsh", |t| {
                t.phase = "done".into();
                t.message = msg.clone();
                t.current = t.latest.clone();
                t.update_available = false;
                t.error = None;
            });
            show_toast(&app, msg);
        }
        Err(e) => {
            update_target(&app, "dsh", |t| {
                t.phase = "error".into();
                t.error = Some(e.clone());
                t.message = format!("dsh 更新失败:{e}");
            });
            show_toast(&app, format!("dsh 更新失败:{e}"));
        }
    });
}

fn update_dsh_inner(app: &tauri::AppHandle, port: u16) -> Result<String, String> {
    // npm on Windows is a .cmd shim, so run it through cmd (also resolves it
    // from PATH) with the hidden/no-console-window flag.
    let current = run_capture(
        "cmd",
        &["/c", "npm", "ls", "-g", "@deepseek-ai/dsh", "--depth=0"],
    )
    .ok()
    .and_then(|out| parse_npm_version(&out));
    let latest = run_capture("cmd", &["/c", "npm", "view", "@deepseek-ai/dsh", "version"])?
        .trim()
        .to_string();
    if latest.is_empty() {
        return Err("无法获取最新版本".into());
    }

    if let Some(cur) = &current {
        if cur == &latest {
            return Ok(format!("dsh 已是最新版本(v{latest})"));
        }
    }

    // Update available: stop, install (hidden), start, navigate.
    update_target(app, "dsh", |t| {
        t.phase = "installing".into();
        t.progress = None;
        t.message = format!("正在安装 dsh v{latest}…");
        t.latest = Some(latest.clone());
        t.error = None;
    });
    stop_dsh(app, port);
    run_capture(
        "cmd",
        &["/c", "npm", "install", "-g", "@deepseek-ai/dsh@latest"],
    )
    .map_err(|e| format!("安装失败:{e}"))?;

    update_target(app, "dsh", |t| {
        t.phase = "restarting".into();
        t.message = "正在重启 dsh 服务…".into();
        t.latest = Some(latest.clone());
    });
    match spawn_server(port) {
        Ok(child) => {
            *app.state::<ServerState>().0.lock().unwrap() = Some(child);
        }
        Err(e) => {
            eprintln!("failed to spawn dsh after update: {e}");
            return Err(format!("更新完成但 dsh 启动失败:{e}"));
        }
    }
    if wait_for_port(port, Duration::from_secs(60)) {
        // The restarted process has a new token; resolve it before navigating.
        if let Some(u) = ensure_auth_url(app, port) {
            if let Ok(parsed) = url::Url::parse(&u) {
                if let Some(content) = app.get_webview("harness-content") {
                    let _ = content.navigate(parsed);
                }
            }
        }
    }

    Ok(match current {
        Some(c) => format!("dsh 已更新:v{c} → v{latest}，服务已重启"),
        None => format!("dsh 已安装 v{latest}，服务已启动"),
    })
}

// --- Startup environment check + auto-install (Node.js / dsh) ---

fn node_available() -> bool {
    run_capture("cmd", &["/c", "where", "node"]).is_ok()
        && run_capture("cmd", &["/c", "where", "npm"]).is_ok()
}

fn node_version() -> String {
    run_capture("cmd", &["/c", "node", "--version"])
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Latest Node.js LTS version (e.g. "v22.14.0") from the official index.
fn latest_node_version() -> Result<String, String> {
    let ps = "$ProgressPreference='SilentlyContinue'\r\n\
         try {\r\n\
         \x20 $r = Invoke-RestMethod -UseBasicParsing -Headers @{ 'User-Agent'='deepseek-harness-desktop' } 'https://nodejs.org/dist/index.json'\r\n\
         \x20 ($r | Where-Object { $_.lts } | Select-Object -First 1).version\r\n\
         } catch { }";
    let out = run_ps(&ps)?;
    let v = out.trim().to_string();
    if v.is_empty() {
        Err("无法获取 Node.js 最新版本(请检查网络)".into())
    } else {
        Ok(v)
    }
}

/// Auto-install Node.js: winget → official MSI → no-admin portable zip.
/// Publishes download progress and log lines to the loading page through `app`.
fn install_node(app: &tauri::AppHandle) -> Result<String, String> {
    // 1) winget (when available)
    if run_capture("cmd", &["/c", "where", "winget"]).is_ok() {
        setup_log(app, "    正在通过 winget 安装 Node.js LTS…");
        let ok = run_status(
            "winget",
            &[
                "install",
                "--id",
                "OpenJS.NodeJS.LTS",
                "--silent",
                "--accept-package-agreements",
                "--accept-source-agreements",
                "--disable-interactivity",
            ],
        )
        .map(|s| s.success())
        .unwrap_or(false);
        if ok {
            refresh_process_path();
            if node_available() {
                return Ok(node_version());
            }
        }
        setup_log(app, "    winget 未成功，改用官方安装包…");
    }

    let version = latest_node_version()?;
    let base_url = format!("https://nodejs.org/dist/{version}");

    // 2) official MSI (silent, machine scope -> UAC prompt)
    match install_node_msi(app, &version, &base_url) {
        Ok(v) => return Ok(v),
        Err(e) => setup_log(app, format!("    官方安装包失败({e})，改用便携版(免管理员)…")),
    }

    // 3) portable zip (no admin rights needed)
    install_node_zip(app, &version, &base_url)
}

fn install_node_msi(app: &tauri::AppHandle, version: &str, base_url: &str) -> Result<String, String> {
    let msi_url = format!("{base_url}/node-{version}-x64.msi");
    let msi = std::env::temp_dir().join(format!("node-{version}-x64.msi"));
    setup_log(app, format!("    正在下载 Node.js {version} 安装包…"));
    let v2 = version.to_string();
    download_with_progress(&msi_url, &msi, &mut |pct| {
        let mut s = app.state::<SharedSetupState>().0.lock().unwrap().clone();
        s.phase = "installing".into();
        s.progress = pct;
        s.message = match pct {
            Some(p) => format!("正在下载 Node.js {v2}… {p:.0}%"),
            None => format!("正在下载 Node.js {v2}…"),
        };
        publish_setup(app, s);
    })?;
    setup_log(app, "    正在静默安装(msiexec /qn)，可能需要几分钟…");
    let status = run_status(
        "msiexec",
        &["/i", msi.to_str().unwrap_or_default(), "/qn", "/norestart"],
    )
    .map_err(|e| format!("启动 Node.js 安装失败:{e}"))?;
    if !(status.success() || status.code() == Some(3010)) {
        return Err(format!("Node.js 安装失败(退出码 {})", status.code().unwrap_or(-1)));
    }
    refresh_process_path();
    if !node_available() {
        return Err("Node.js 安装完成但仍无法在 PATH 中找到 node/npm".into());
    }
    Ok(node_version())
}

fn install_node_zip(app: &tauri::AppHandle, version: &str, base_url: &str) -> Result<String, String> {
    let zip_url = format!("{base_url}/node-{version}-win-x64.zip");
    let zip_file = std::env::temp_dir().join(format!("node-{version}-win-x64.zip"));
    setup_log(app, format!("    正在下载便携版 Node.js {version}…"));
    let v2 = version.to_string();
    download_with_progress(&zip_url, &zip_file, &mut |pct| {
        let mut s = app.state::<SharedSetupState>().0.lock().unwrap().clone();
        s.phase = "installing".into();
        s.progress = pct;
        s.message = match pct {
            Some(p) => format!("正在下载 Node.js {v2} 便携版… {p:.0}%"),
            None => format!("正在下载 Node.js {v2} 便携版…"),
        };
        publish_setup(app, s);
    })?;
    let dest = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    let dest = std::path::PathBuf::from(dest).join("Programs").join("nodejs");
    std::fs::create_dir_all(&dest).map_err(|e| format!("创建安装目录失败:{e}"))?;
    setup_log(app, "    正在解压并配置…");
    let status = run_status(
        "tar",
        &[
            "-xf",
            zip_file.to_str().unwrap_or_default(),
            "-C",
            dest.to_str().unwrap_or_default(),
            "--strip-components=1",
        ],
    )
    .map_err(|e| format!("解压 Node.js 失败:{e}"))?;
    if !status.success() {
        return Err("解压 Node.js 失败".into());
    }
    add_user_path(dest.display().to_string());
    refresh_process_path();
    if !node_available() {
        return Err("便携版 Node.js 未能生效".into());
    }
    Ok(node_version())
}

/// Installed dsh version (global npm prefix, falling back to the user prefix
/// used when a global install was not permitted).
fn dsh_version() -> Option<String> {
    let out = run_capture("cmd", &["/c", "npm", "ls", "-g", "@deepseek-ai/dsh", "--depth=0"]).ok()?;
    if let Some(v) = parse_npm_version(&out) {
        return Some(v);
    }
    let prefix = user_npm_prefix();
    let prefix = prefix.to_str()?;
    let out = run_capture(
        "cmd",
        &["/c", "npm", "ls", "-g", "--prefix", prefix, "@deepseek-ai/dsh", "--depth=0"],
    )
    .ok()?;
    parse_npm_version(&out)
}

/// Auto-install dsh globally; falls back to a user-directory prefix when the
/// global install fails (no admin rights).
fn install_dsh(app: &tauri::AppHandle) -> Result<String, String> {
    setup_log(app, "    正在运行 npm install -g @deepseek-ai/dsh@latest（可能需要几分钟）…");
    match run_capture("cmd", &["/c", "npm", "install", "-g", "@deepseek-ai/dsh@latest"]) {
        Ok(_) => {
            let v = dsh_version().unwrap_or_else(|| "unknown".into());
            Ok(format!("v{v}"))
        }
        Err(e) => {
            setup_log(app, format!("    全局安装失败({e})，改用用户目录安装…"));
            let prefix = user_npm_prefix();
            if std::fs::create_dir_all(&prefix).is_err() {
                return Err(format!("无法创建用户目录 {}", prefix.display()));
            }
            let prefix_str = prefix.to_string_lossy().into_owned();
            let cmd = format!("npm install -g --prefix \"{prefix_str}\" @deepseek-ai/dsh@latest");
            run_capture("cmd", &["/c", cmd.as_str()])
                .map_err(|e2| format!("全局安装失败({e})，用户目录安装也失败({e2})"))?;
            add_user_path(prefix_str);
            refresh_process_path();
            let v = dsh_version().unwrap_or_else(|| "unknown".into());
            Ok(format!("v{v}(用户目录)"))
        }
    }
}

fn navigate_to(app: &tauri::AppHandle, url: &str) {
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(content) = app.get_webview("harness-content") {
            let _ = content.navigate(parsed);
        }
    }
}

/// Mark `key` failed, publish the error state (the loading page then shows the
/// failure + a retry button), and return Err for the caller to bail out.
fn setup_error(
    app: &tauri::AppHandle,
    mut state: SetupState,
    key: &str,
    title: &str,
    detail: String,
) -> Result<(), String> {
    set_setup_item(&mut state, key, "failed", detail.clone());
    state.phase = "error".into();
    state.message = format!("{title}:{detail}");
    state.progress = None;
    publish_setup(app, state);
    Err(detail)
}

/// Full startup flow: detect + auto-install missing Node.js/dsh, start the dsh
/// server and navigate to it. Runs on a background thread and publishes live
/// progress to the loading page (`setup-progress` / `setup-log`). Guarded by
/// SetupRunning so only one run is active at a time.
fn setup_and_start(app: tauri::AppHandle, port: u16) {
    {
        let st = app.state::<SetupRunning>();
        let mut guard = st.0.lock().unwrap();
        if *guard {
            return;
        }
        *guard = true;
    }
    let ok = setup_and_start_inner(&app, port).is_ok();
    *app.state::<SetupRunning>().0.lock().unwrap() = false;
    // 自动更新设置勾选时:启动就绪后后台检查并更新 dsh / 桌面程序。
    if ok {
        maybe_auto_update(app, port);
    }
}

fn setup_and_start_inner(app: &tauri::AppHandle, port: u16) -> Result<(), String> {
    let mut state = default_setup_state();

    // Fast path: dsh is already running on the port — connect, resolving the
    // per-process access token from the log / persisted URL; when none is
    // valid, restart the service so we own it and can read its token.
    if port_open(port) {
        setup_log(app, "检测到 dsh 服务已在运行，连接中…");
        set_setup_item(&mut state, "webview2", "ok", "已就绪");
        set_setup_item(&mut state, "node", "ok", "已就绪");
        set_setup_item(&mut state, "dsh", "ok", "已就绪");
        set_setup_item(&mut state, "service", "installed", format!("http://127.0.0.1:{port}"));
        return match ensure_auth_url(app, port) {
            Some(u) => {
                state.phase = "ready".into();
                state.message = "启动完成".into();
                publish_setup(app, state.clone());
                navigate_to(app, &u);
                Ok(())
            }
            None => {
                let msg = "无法获取 dsh 访问令牌".to_string();
                setup_log(app, format!("  {msg}"));
                setup_error(app, state, "service", "dsh 服务启动失败", msg)
            }
        };
    }

    publish_setup(app, state.clone());
    setup_log(app, "开始检查运行环境…");

    // WebView2 was ensured in the native pre-flight phase before any webview.
    set_setup_item(&mut state, "webview2", "ok", "已就绪");
    publish_setup(app, state.clone());

    // Node.js
    setup_log(app, "[1/3] 检测 Node.js…");
    set_setup_item(&mut state, "node", "checking", "检测中…");
    publish_setup(app, state.clone());
    if node_available() {
        let v = node_version();
        setup_log(app, format!("  已就绪({v})"));
        set_setup_item(&mut state, "node", "ok", v);
    } else {
        setup_log(app, "  未检测到 Node.js，开始自动安装…");
        set_setup_item(&mut state, "node", "installing", "正在自动安装…");
        state.phase = "installing".into();
        state.message = "正在安装 Node.js…".into();
        publish_setup(app, state.clone());
        match install_node(app) {
            Ok(v) => {
                setup_log(app, format!("  Node.js 安装完成({v})"));
                set_setup_item(&mut state, "node", "installed", v);
            }
            Err(e) => return setup_error(app, state, "node", "Node.js 安装失败", e),
        }
        publish_setup(app, state.clone());
    }

    // dsh
    setup_log(app, "[2/3] 检测 dsh…");
    set_setup_item(&mut state, "dsh", "checking", "检测中…");
    publish_setup(app, state.clone());
    if let Some(v) = dsh_version() {
        setup_log(app, format!("  已就绪(v{v})"));
        set_setup_item(&mut state, "dsh", "ok", format!("v{v}"));
    } else {
        setup_log(app, "  未检测到 dsh，开始自动安装…");
        set_setup_item(&mut state, "dsh", "installing", "正在自动安装…");
        state.phase = "installing".into();
        state.message = "正在安装 dsh…".into();
        publish_setup(app, state.clone());
        match install_dsh(app) {
            Ok(v) => {
                setup_log(app, format!("  dsh 安装完成({v})"));
                set_setup_item(&mut state, "dsh", "installed", v);
            }
            Err(e) => return setup_error(app, state, "dsh", "dsh 安装失败", e),
        }
        publish_setup(app, state.clone());
    }

    // Start the dsh server.
    setup_log(app, "[3/3] 启动 dsh 服务…");
    set_setup_item(&mut state, "service", "ok", "正在启动…");
    state.phase = "starting".into();
    state.message = "正在启动 dsh 服务…".into();
    state.progress = None;
    publish_setup(app, state.clone());

    if !port_open(port) {
        match spawn_server(port) {
            Ok(child) => {
                *app.state::<ServerState>().0.lock().unwrap() = Some(child);
            }
            Err(e) => {
                let msg = format!("启动 dsh 服务进程失败:{e}");
                setup_log(app, format!("  {msg}"));
                return setup_error(app, state, "service", "dsh 服务启动失败", msg);
            }
        }
    }

    // Wait for the port, failing fast when the spawned server process dies and
    // showing live "已等待 Xs" progress so the UI never looks frozen.
    if wait_server_ready(app, port, Duration::from_secs(180)) {
        setup_log(app, format!("  服务已就绪(http://127.0.0.1:{port})"));
        set_setup_item(&mut state, "service", "installed", format!("http://127.0.0.1:{port}"));
        // Resolve the authenticated URL (the `dsh web:` token line) before
        // navigating; without the token the new dsh returns 401.
        match ensure_auth_url(app, port) {
            Some(u) => {
                state.phase = "ready".into();
                state.message = "启动完成".into();
                state.progress = None;
                publish_setup(app, state.clone());
                navigate_to(app, &u);
                Ok(())
            }
            None => {
                let msg = "无法获取 dsh 访问令牌".to_string();
                setup_log(app, format!("  {msg}"));
                setup_error(app, state, "service", "dsh 服务启动失败", msg)
            }
        }
    } else {
        let tail = log_tail(port, 10);
        let msg = if tail.is_empty() {
            format!("dsh 服务启动失败(端口 {port} 未就绪)")
        } else {
            format!("dsh 服务启动失败，最近输出：\n{tail}")
        };
        setup_log(app, format!("  {msg}"));
        setup_error(app, state, "service", "dsh 服务启动失败", msg)
    }
}

/// Manual retry of the whole environment check / auto-install / start flow
/// (loading page "重试" button).
#[tauri::command]
fn retry_setup(app: tauri::AppHandle) {
    if about_active(&app) {
        show_toast(&app, "更新正在进行中，请稍后再试");
        return;
    }
    if *app.state::<SetupRunning>().0.lock().unwrap() {
        show_toast(&app, "环境检测/安装正在进行中");
        return;
    }
    let port = active_port();
    std::thread::spawn(move || setup_and_start(app, port));
}

/// Current setup state snapshot (loading page restores it on load / retry).
#[tauri::command]
fn get_setup_state(app: tauri::AppHandle) -> SetupState {
    app.state::<SharedSetupState>().0.lock().unwrap().clone()
}


// --- 关于 (About): app version + update check / update via GitHub releases ---

/// Minimal fields of the latest GitHub release that we need, extracted by the
/// PowerShell helper (so we never parse GitHub's full payload here).
struct LatestRelease {
    tag_name: String,
    body: Option<String>,
    asset_url: String,
}

/// Write a PowerShell script to a temp file and run it hidden, returning its
/// stdout. Using `-File` avoids the command-line quoting pitfalls of passing a
/// multi-line script with quotes through `powershell -Command`.
fn run_ps(script: &str) -> Result<String, String> {
    let tmp_dir = std::env::temp_dir();
    let ps = tmp_dir.join(format!("dsh-update-{}.ps1", std::process::id()));
    std::fs::write(&ps, script).map_err(|e| format!("写入临时脚本失败:{e}"))?;
    let result = run_capture(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            ps.to_str().unwrap_or_default(),
        ],
    );
    let _ = std::fs::remove_file(&ps);
    result
}

/// Fetch the latest release metadata via PowerShell (Invoke-RestMethod).
/// GitHub requires a User-Agent header, otherwise the request is rejected
/// with 403. A 404 (no release published yet) is reported distinctly so the
/// UI can treat it as "up to date"; other failures return a Chinese message.
fn fetch_latest_release() -> Result<LatestRelease, String> {
    // Prints a compact JSON: { ok, tag, body, asset } or { ok=false, code|message }.
    let ps = format!(
        "$ProgressPreference='SilentlyContinue'\r\n\
         try {{\r\n\
         \x20 $r = Invoke-RestMethod -UseBasicParsing -Headers @{{ 'User-Agent'='deepseek-harness-desktop ({GITHUB_REPO})' }} '{GITHUB_LATEST_API}'\r\n\
         \x20 $a = $r.assets | Where-Object {{ $_.name -ieq 'deepseek-harness.exe' }} | Select-Object -First 1\r\n\
         \x20 if (-not $a) {{ $a = $r.assets | Where-Object {{ $_.name -like '*.exe' }} | Select-Object -First 1 }}\r\n\
         \x20 [pscustomobject]@{{ ok=$true; tag=$r.tag_name; body=$r.body; asset=$a.browser_download_url }} | ConvertTo-Json -Compress\r\n\
         }} catch {{\r\n\
         \x20 $code = 0\r\n\
         \x20 try {{ $code = $_.Exception.Response.StatusCode.value__ }} catch {{}}\r\n\
         \x20 if ($code -eq 404) {{ [pscustomobject]@{{ ok=$false; code=404 }} | ConvertTo-Json -Compress }}\r\n\
         \x20 else {{ [pscustomobject]@{{ ok=$false; code=0; message=($_.Exception.Message -replace '\\r?\\n',' ') }} | ConvertTo-Json -Compress }}\r\n\
         }}"
    );
    let out = run_ps(&ps)?;
    let v: serde_json::Value =
        serde_json::from_str(out.trim()).map_err(|e| format!("解析更新信息失败:{e}"))?;
    if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
        Ok(LatestRelease {
            tag_name: v["tag"].as_str().unwrap_or_default().to_string(),
            body: v["body"].as_str().map(|s| s.to_string()),
            asset_url: v["asset"].as_str().unwrap_or_default().to_string(),
        })
    } else if v["code"].as_i64() == Some(404) {
        Err("暂无已发布版本".into())
    } else {
        Err(format!(
            "获取 GitHub 版本信息失败:{}",
            v["message"].as_str().unwrap_or("未知错误")
        ))
    }
}

/// Normalize a version string ("v1.2.3" / "1.2.3") into semver for comparison.
fn parse_semver(s: &str) -> Option<semver::Version> {
    semver::Version::parse(s.trim().trim_start_matches('v')).ok()
}

/// Desktop app info shown in the 关于 panel: version + repo + installed dsh
/// version. `repo` comes from the GITHUB_REPO constant so the UI never drifts
/// from the backend; `dsh_version` is None when dsh is not installed.
///
/// `rename_all = "camelCase"` is required here: the title bar reads
/// `info.dshVersion`, and without it serde would serialize the field as
/// `dsh_version`, which the JS reads as undefined and renders as "未安装".
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    version: String,
    repo: String,
    dsh_version: Option<String>,
}

#[tauri::command]
fn get_app_info(app: tauri::AppHandle) -> AppInfo {
    AppInfo {
        version: app.package_info().version.to_string(),
        repo: GITHUB_REPO.to_string(),
        dsh_version: dsh_version(),
    }
}

/// A dsh update is due when dsh is not installed yet, or the npm latest is
/// strictly newer than the installed version. A failed npm query (empty
/// `latest`) never counts as an update.
fn dsh_should_update(current: Option<&str>, latest: &str) -> bool {
    match (current, parse_semver(latest)) {
        (None, Some(_)) => true,
        (Some(c), Some(l)) => parse_semver(c).map_or(true, |c| l > c),
        _ => false,
    }
}

/// A desktop-app update is due when the latest GitHub release version differs
/// from the running version (same rule as the pre-existing 关于 check).
fn app_should_update(current: &str, latest_raw: &str) -> bool {
    matches!(
        (parse_semver(latest_raw), parse_semver(current)),
        (Some(l), Some(c)) if l != c
    )
}

/// Check the dsh target (installed vs npm latest) and publish the result to
/// the dsh row of the 关于 panel. Runs on its own thread inside `check_updates`.
fn check_dsh_target(app: &tauri::AppHandle) {
    update_target(app, "dsh", |t| {
        t.phase = "checking".into();
        t.message = "正在检查 dsh 更新…".into();
        t.progress = None;
        t.error = None;
    });
    let current = dsh_version();
    let latest = npm_latest_dsh();
    if latest.is_empty() {
        update_target(app, "dsh", |t| {
            t.phase = "error".into();
            t.error = Some("无法获取最新版本".into());
            t.message = "检查 dsh 更新失败:无法获取最新版本".into();
        });
        return;
    }
    let update_available = dsh_should_update(current.as_deref(), &latest);
    let message = if update_available {
        match &current {
            Some(c) => format!("发现新版本 v{c} → v{latest}"),
            None => format!("发现新版本 v{latest}（尚未安装 dsh）"),
        }
    } else {
        format!("已是最新版本 v{}", current.as_deref().unwrap_or("?"))
    };
    update_target(app, "dsh", |t| {
        t.phase = "done".into();
        t.current = current;
        t.latest = Some(latest);
        t.update_available = update_available;
        t.message = message;
        t.error = None;
    });
}

/// Check the desktop-app target (running version vs latest GitHub release) and
/// publish the result to the app row of the 关于 panel. Runs on its own thread
/// inside `check_updates`.
fn check_app_target(app: &tauri::AppHandle) {
    update_target(app, "app", |t| {
        t.phase = "checking".into();
        t.message = "正在检查桌面版更新…".into();
        t.progress = None;
        t.error = None;
    });
    let current = app.package_info().version.to_string();
    match fetch_latest_release() {
        Ok(release) => {
            let latest_raw = release.tag_name.trim().trim_start_matches('v').to_string();
            let update_available = app_should_update(&current, &latest_raw);
            let message = if update_available {
                format!("发现新版本 v{latest_raw}")
            } else {
                format!("已是最新版本 v{current}")
            };
            update_target(app, "app", |t| {
                t.phase = "done".into();
                t.current = Some(current);
                t.latest = Some(latest_raw);
                t.update_available = update_available;
                t.release_notes = release.body.unwrap_or_default();
                t.asset_url = release.asset_url;
                t.message = message;
                t.error = None;
            });
        }
        Err(e) => update_target(app, "app", |t| {
            t.phase = "error".into();
            t.error = Some(e.clone());
            t.message = format!("检查桌面版更新失败:{e}");
        }),
    }
}

/// The 关于 panel's per-target 检查更新 buttons: each card (dsh / 桌面版) checks
/// only its own target, so the two checks run independently (and can overlap).
/// Progress is broadcast via the `about-progress` event. An unknown target is a
/// no-op, and each target is guarded individually by its own in-flight phase.
#[tauri::command]
fn check_updates(app: tauri::AppHandle, target: String) {
    match target.as_str() {
        "dsh" => {
            {
                let st = app.state::<SharedAboutState>().0.lock().unwrap().clone();
                if target_active(&st.dsh) {
                    show_toast(&app, "dsh 检查/更新正在进行");
                    return;
                }
            }
            std::thread::spawn(move || check_dsh_target(&app));
        }
        "app" => {
            {
                let st = app.state::<SharedAboutState>().0.lock().unwrap().clone();
                if target_active(&st.app) {
                    show_toast(&app, "桌面版检查/更新正在进行");
                    return;
                }
            }
            std::thread::spawn(move || check_app_target(&app));
        }
        _ => {}
    }
}

/// Open an external URL in the system default browser (used by the repo link
/// in the 关于 dialog and by the content webviews' external-link handlers).
fn open_in_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("cmd")
            .args(["/c", "start", "", url])
            .creation_flags(0x0800_0000)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
}

/// Open an external URL in the system default browser (title bar About dialog
/// repo link). The bar webview itself never navigates away.
#[tauri::command]
fn open_url(url: String) {
    open_in_browser(&url);
}

/// True when `url` is safe to load inside the app's own webviews:
/// the dsh server origin (`127.0.0.1` / `localhost` on the active port —
/// including the tokenized `?token=` URL dsh redirects to) and the app's own
/// local assets (`http://tauri.localhost/...`). Any other http(s) URL is
/// treated as external and handed to the system browser instead. Non-http(s)
/// URLs (`data:`, `blob:`, `about:`, `tauri://`, ...) always stay in-webview.
fn is_internal_url(url: &url::Url, port: u16) -> bool {
    if matches!(url.scheme(), "http" | "https") {
        match url.host_str() {
            Some("tauri.localhost") => true,
            Some("127.0.0.1") | Some("localhost") => {
                url.port().map_or(true, |p| p == port)
            }
            _ => false,
        }
    } else {
        true
    }
}

/// Portable-exe self update of the desktop app
/// (关于 -> 桌面版卡片 -> "更新桌面版"). Downloads the new exe from the latest
/// GitHub release (hidden PowerShell, polled for real byte progress), then
/// hands over to a detached helper that waits for this process to exit,
/// replaces the running exe and relaunches it. Runs on a background thread so
/// the 关于 panel can be closed and reopened without losing the update status.
#[tauri::command]
fn update_app(app: tauri::AppHandle) -> Result<(), String> {
    {
        let st = app.state::<SharedAboutState>().0.lock().unwrap().clone();
        if target_active(&st.app) {
            return Err("桌面版更新正在进行".into());
        }
    }
    std::thread::spawn(move || {
        if let Err(e) = update_app_inner(&app) {
            update_target(&app, "app", |t| {
                t.phase = "error".into();
                t.error = Some(e.clone());
                t.message = format!("更新桌面版失败:{e}");
            });
        }
    });
    Ok(())
}

fn update_app_inner(app: &tauri::AppHandle) -> Result<(), String> {
    update_target(app, "app", |t| {
        t.phase = "checking".into();
        t.message = "正在获取版本信息…".into();
        t.progress = None;
        t.error = None;
    });
    let release = fetch_latest_release()?;
    if release.asset_url.is_empty() {
        return Err("该发布中没有可用的 exe 更新包".into());
    }
    let tmp_exe = download_app_update(app, &release)?;
    finalize_app_update(app, &tmp_exe)
}

/// Download phase of the portable-exe self update: fetch the new exe from the
/// latest GitHub release to a temp location (hidden PowerShell, polled for real
/// byte progress) and return its path. Unlike `update_app_inner` it does NOT
/// exit the process, so the startup auto-update can keep the app alive until a
/// concurrently running dsh update has settled.
fn download_app_update(
    app: &tauri::AppHandle,
    release: &LatestRelease,
) -> Result<std::path::PathBuf, String> {
    let current_exe = std::env::current_exe().map_err(|e| format!("无法定位当前程序路径:{e}"))?;
    let exe_name = current_exe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("deepseek-harness.exe");

    // Download the new portable exe to a temp location. First clear any stale
    // partial from a previous run, then (best-effort) get the total size and
    // spawn the download as a hidden background process whose partial output
    // file we poll for real byte progress.
    let tmp_dir = std::env::temp_dir();
    let tmp_exe = tmp_dir.join(format!("{exe_name}.update.exe"));
    let _ = std::fs::remove_file(&tmp_exe);
    let total = head_content_length(&release.asset_url);

    let log = tmp_dir.join(format!("{exe_name}.download-error.log"));
    let ps_file = tmp_dir.join(format!("dsh-download-{}.ps1", std::process::id()));
    let ps = format!(
        "$ProgressPreference='SilentlyContinue'\r\n\
         try {{\r\n\
         \x20 Invoke-WebRequest -UseBasicParsing -Headers @{{ 'User-Agent'='deepseek-harness-desktop ({GITHUB_REPO})' }} -OutFile '{tmp}' '{url}'\r\n\
         \x20 exit 0\r\n\
         }} catch {{\r\n\
         \x20 $_.Exception.Message | Out-File -FilePath '{err}' -Encoding utf8\r\n\
         \x20 exit 1\r\n\
         }}",
        tmp = tmp_exe.display(),
        url = release.asset_url,
        err = log.display(),
    );
    std::fs::write(&ps_file, &ps).map_err(|e| format!("写入下载脚本失败:{e}"))?;
    let mut child = spawn_ps_hidden(&ps_file, &log).map_err(|e| format!("启动下载失败:{e}"))?;

    update_target(app, "app", |t| {
        t.phase = "downloading".into();
        t.progress = Some(0.0);
        t.message = "正在下载更新包…".into();
    });

    // Poll the partial file size to drive the progress bar; throttle updates
    // to whole-percent changes so the event stream stays light.
    let mut last_pct = -1.0f64;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = std::fs::remove_file(&ps_file);
                if !status.success() {
                    let msg = read_file(&log);
                    let _ = std::fs::remove_file(&log);
                    return Err(if msg.trim().is_empty() {
                        "下载更新包失败".into()
                    } else {
                        msg.trim().to_string()
                    });
                }
                break;
            }
            Ok(None) => {
                let len = std::fs::metadata(&tmp_exe).map(|m| m.len()).unwrap_or(0);
                if let Some(total) = total {
                    if total > 0 {
                        let pct = (len as f64 / total as f64 * 100.0).min(100.0);
                        if (pct - last_pct).abs() >= 1.0 {
                            last_pct = pct;
                            update_target(app, "app", |t| {
                                t.phase = "downloading".into();
                                t.progress = Some(pct);
                                t.message = format!("正在下载更新包… {pct:.0}%");
                            });
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => {
                let _ = std::fs::remove_file(&ps_file);
                return Err("无法读取下载进程状态".into());
            }
        }
    }

    let len = std::fs::metadata(&tmp_exe).map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        return Err("下载结果为空，更新失败".into());
    }
    Ok(tmp_exe)
}

/// Finalize phase of the portable-exe self update: write a detached helper that
/// waits for this process to exit, swaps the running exe and relaunches it,
/// then quit the app (the helper's process tree survives the exit). Used by
/// both the manual 关于 "更新桌面版" button and the startup auto-update.
fn finalize_app_update(app: &tauri::AppHandle, tmp_exe: &std::path::Path) -> Result<(), String> {
    let current_exe = std::env::current_exe().map_err(|e| format!("无法定位当前程序路径:{e}"))?;
    let exe_name = current_exe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("deepseek-harness.exe");

    update_target(app, "app", |t| {
        t.phase = "finalizing".into();
        t.progress = Some(100.0);
        t.message = "正在替换程序并重启…".into();
    });

    // Write a detached helper that waits for us to exit, swaps the exe and
    // relaunches it. It keeps running after the parent (this app) exits, so it
    // can overwrite the file our own process no longer locks.
    let tmp_dir = std::env::temp_dir();
    let log = tmp_dir.join(format!("{exe_name}.update-helper.log"));
    let helper = tmp_dir.join(format!("{exe_name}.update-helper.bat"));
    let bat = format!(
        "@echo off\r\n\
         setlocal\r\n\
         :wait\r\n\
         tasklist /FI \"IMAGENAME eq {exe_name}\" 2>nul | find /I \"{exe_name}\" >nul\r\n\
         if not errorlevel 1 (timeout /t 1 /nobreak >nul & goto wait)\r\n\
         copy /y \"{tmp}\" \"{target}\" >nul\r\n\
         if errorlevel 1 (echo update-copy-failed > \"{log}\" & exit /b 1)\r\n\
         start \"\" \"{target}\"\r\n\
         del /q \"{tmp}\" >nul 2>nul\r\n\
         exit /b 0",
        exe_name = exe_name,
        tmp = tmp_exe.display(),
        target = current_exe.display(),
        log = log.display(),
    );
    std::fs::write(&helper, bat).map_err(|e| format!("写入更新脚本失败:{e}"))?;

    // Spawn the helper detached (CREATE_NO_WINDOW) and quit. The helper's
    // process tree survives this process exiting.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("cmd")
            .args(["/c", helper.to_str().unwrap_or_default()])
            .creation_flags(0x0800_0000)
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = Command::new("sh").arg(&helper).spawn();
    }
    flush_window_state(app);
    app.exit(0);
    Ok(())
}

// --- 启动自动更新 (自动更新 setting: check + update dsh / desktop app) ---

/// Latest published dsh version on the npm registry ("" on failure).
fn npm_latest_dsh() -> String {
    run_capture("cmd", &["/c", "npm", "view", "@deepseek-ai/dsh", "version"])
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Startup auto-update (自动更新 setting): check + update dsh (npm) and the
/// desktop app (GitHub release) in parallel background threads, so neither
/// target is ever skipped by the other. The desktop-app self update normally
/// ends by exiting the process (the detached helper swaps the exe), so its
/// final exit waits for the dsh flow to settle first — otherwise the process
/// exit would cut a concurrent dsh npm install / server restart short. Stays
/// silent when everything is current; only real updates (or dsh failures)
/// surface a toast.
fn maybe_auto_update(app: tauri::AppHandle, port: u16) {
    if !load_settings(&app).auto_update {
        return;
    }
    if *app.state::<SetupRunning>().0.lock().unwrap() {
        return;
    }
    if about_active(&app) {
        return;
    }

    // Set once the dsh check/update flow has fully finished. The desktop-app
    // thread waits on it before exiting the process, so the self-update
    // restart never interrupts a dsh update that is still in flight.
    let dsh_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Thread A: dsh (npm) — update when not installed yet or when a newer
    // version exists. Runs fully independently of the desktop-app update.
    {
        let app = app.clone();
        let done = dsh_done.clone();
        std::thread::spawn(move || {
            let current = dsh_version();
            let latest = npm_latest_dsh();
            if dsh_should_update(current.as_deref(), &latest) {
                match update_dsh_inner(&app, port) {
                    Ok(msg) => {
                        update_target(&app, "dsh", |t| {
                            t.phase = "done".into();
                            t.message = msg.clone();
                            t.current = t.latest.clone();
                            t.update_available = false;
                            t.error = None;
                        });
                        show_toast(&app, msg);
                    }
                    Err(e) => {
                        update_target(&app, "dsh", |t| {
                            t.phase = "error".into();
                            t.error = Some(e.clone());
                            t.message = format!("自动更新 dsh 失败:{e}");
                        });
                        show_toast(&app, format!("自动更新 dsh 失败:{e}"));
                    }
                }
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }

    // Thread B: desktop app (GitHub release) — update only when strictly newer.
    std::thread::spawn(move || {
        match fetch_latest_release() {
            Ok(release) => {
                let current = app.package_info().version.to_string();
                let latest_raw = release.tag_name.trim().trim_start_matches('v').to_string();
                if app_should_update(&current, &latest_raw) {
                    show_toast(&app, "发现新版本，正在后台更新应用…");
                    // Download first (this process must stay alive while the
                    // concurrent dsh update runs), then wait for dsh to settle
                    // before the final exit that swaps the exe.
                    let tmp_exe = match download_app_update(&app, &release) {
                        Ok(p) => p,
                        Err(e) => {
                            update_target(&app, "app", |t| {
                                t.phase = "error".into();
                                t.error = Some(e.clone());
                                t.message = format!("自动更新应用失败:{e}");
                            });
                            show_toast(&app, format!("自动更新应用失败:{e}"));
                            return;
                        }
                    };
                    // Bound the wait so a hung dsh flow can never block the
                    // app restart forever; an interrupted dsh update is
                    // retried on the next launch.
                    let wait_deadline = std::time::Instant::now() + Duration::from_secs(600);
                    while !dsh_done.load(std::sync::atomic::Ordering::SeqCst)
                        && std::time::Instant::now() < wait_deadline
                    {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    if let Err(e) = finalize_app_update(&app, &tmp_exe) {
                        update_target(&app, "app", |t| {
                            t.phase = "error".into();
                            t.error = Some(e.clone());
                            t.message = format!("自动更新应用失败:{e}");
                        });
                        show_toast(&app, format!("自动更新应用失败:{e}"));
                    }
                    // finalize_app_update ends with app.exit(0) on success.
                }
            }
            Err(_) => {} // 启动时网络失败保持安静
        }
    });
}

pub fn run() {
    let port = active_port();

    // --- pre-flight (before any webview can exist): ensure WebView2 ---
    // Without the WebView2 runtime no webview can be created, so this must
    // happen here, natively. If it is missing we install it via the official
    // Evergreen bootstrapper, whose own window shows the real download/install
    // progress (a silent install would hide it). May also trigger a UAC prompt;
    // on failure we can only tell the user via a native dialog and exit.
    if !webview2_installed() {
        native_msg(
            "需要 WebView2 运行时",
            "DeepSeek Harness Desktop 依赖 Microsoft WebView2 运行时。\n\n\
             当前未检测到该组件，将自动下载并安装（约 100MB，会弹出系统管理员授权，请点击“是”）。\n\n\
             随后会打开 WebView2 运行时安装窗口，请在窗口中等待下载安装完成（完成后点“关闭”即可），应用会自动继续启动。",
            false,
        );
        if let Err(e) = install_webview2() {
            native_msg(
                "WebView2 安装失败",
                &format!(
                    "自动安装 WebView2 运行时失败:{e}\n\n\
                     请手动下载并安装后重新运行本应用:\n\
                     https://developer.microsoft.com/microsoft-edge/webview2/"
                ),
                true,
            );
            std::process::exit(1);
        }
    }
    // Merge machine + user PATH from the registry into this process, so tools
    // this app installs (Node.js, npm, user-prefix dsh) are found by its
    // children without restarting.
    refresh_process_path();

    let global_shortcut_plugin = tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, _shortcut, event| {
            if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                toggle_devtools(app.clone());
            }
        })
        .build();

    tauri::Builder::default()
        .plugin(global_shortcut_plugin)
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Second launch: focus the single main window and exit.
            if let Some(window) = app.get_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .manage(ServerState(Mutex::new(None)))
        .manage(BarHeight(Mutex::new(TITLE_BAR_HEIGHT)))
        .manage(CurrentTarget(Mutex::new("harness".to_string())))
        .manage(SharedAboutState(Mutex::new(AboutState::default())))
        .manage(SetupRunning(Mutex::new(false)))
        .manage(SharedSetupState(Mutex::new(default_setup_state())))
        .manage(WindowStateStore(Mutex::new(WindowStateCache {
            pending: None,
            last_saved: None,
        })))
        .invoke_handler(tauri::generate_handler![
            switch_to,
            quit,
            set_bar_height,
            stop_dsh_cmd,
            quit_with_dsh,
            restart_dsh,
            update_dsh,
            update_app,
            check_updates,
            get_app_info,
            get_about_state,
            open_url,
            retry_setup,
            get_setup_state,
            get_settings,
            save_settings
        ])
        .setup(move |app| {
            let app_handle = app.handle().clone();

            // --- single main window: persistent title bar + two content
            // webviews that are toggled by switch_to ---
            // The window is built hidden and configured (geometry restore +
            // theme) before the first show, so there is no light->dark flash
            // and the loading page is centered at the final size from its
            // very first frame (no startup "jitter"). A saved maximized state
            // is deferred: add_webviews maximizes only once the first page
            // has finished loading, so the window never pops in maximized
            // but empty (no startup flash).
            let window = tauri::window::WindowBuilder::new(app, "main")
                .title("DeepSeek Harness")
                .inner_size(1280.0, 720.0)
                .decorations(false)
                .resizable(true)
                .background_color(LIGHT_BG)
                .visible(false)
                .build()?;
            // Restore the last window position / size (or maximize), falling
            // back to centered defaults on first run or when the saved
            // position is off-screen (e.g. a monitor was unplugged).
            let restore_maximized = restore_window_geometry(&window, &app_handle);
            add_webviews(window.clone(), app_handle.clone(), restore_maximized)?;
            let _ = window.show();

            // F12 toggles DevTools on the currently visible content webview.
            // Registration can fail if another app already owns F12; that
            // must not prevent the app from starting.
            use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut};
            if let Err(_e) = app
                .global_shortcut()
                .register(Shortcut::new(None, Code::F12))
            {
                //eprintln!("failed to register F12 global shortcut: {e}");
            }

            // --- auto environment check + install + start (background) ---
            // The loading page renders immediately; every step of the check /
            // install / start is published to it via `setup-progress` /
            // `setup-log`, so a beginner sees live progress and details, and on
            // failure gets an actionable error with a retry button.
            std::thread::spawn(move || setup_and_start(app_handle.clone(), port));

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_, _| {});
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_global_npm_dsh_version() {
        assert_eq!(
            parse_npm_version("`-- @deepseek-ai/dsh@0.1.0-rc.7\n"),
            Some("0.1.0-rc.7".into())
        );
        assert_eq!(
            parse_npm_version("  @deepseek-ai/dsh@1.2.3 deduped\n"),
            Some("1.2.3".into())
        );
    }

    #[test]
    fn parse_npm_version_absent() {
        assert_eq!(parse_npm_version(""), None);
        assert_eq!(parse_npm_version("`-- some-other-pkg@1.0.0\n"), None);
    }

    #[test]
    fn dsh_should_update_logic() {
        // Not installed + a valid latest -> update is due.
        assert!(dsh_should_update(None, "1.2.3"));
        // Same version -> no update.
        assert!(!dsh_should_update(Some("1.2.3"), "1.2.3"));
        // Newer npm version -> update.
        assert!(dsh_should_update(Some("1.2.3"), "1.3.0"));
        // Pre-release installed vs stable latest -> update.
        assert!(dsh_should_update(Some("0.1.0-rc.7"), "0.1.0"));
        // Installed newer than the registry (downgrade) -> no update.
        assert!(!dsh_should_update(Some("2.0.0"), "1.5.0"));
        // Failed npm query (empty latest) -> never counts as an update.
        assert!(!dsh_should_update(None, ""));
        assert!(!dsh_should_update(Some("1.0.0"), "not-a-version"));
    }

    #[test]
    fn app_should_update_logic() {
        assert!(app_should_update("1.0.4", "1.1.0"));
        assert!(!app_should_update("1.0.4", "1.0.4"));
        // Any difference counts (same rule as the pre-existing 关于 check).
        assert!(app_should_update("1.0.4", "1.0.3"));
    }

    #[test]
    fn app_info_serializes_camel_case_fields() {
        // Regression: the title bar reads info.dshVersion; without
        // rename_all = "camelCase" the field serializes as dsh_version and the
        // JS renders "未安装" even when dsh is installed.
        let info = AppInfo {
            version: "1.0.4".into(),
            repo: "wq3333/deepseek-harness-desktop".into(),
            dsh_version: Some("0.1.5-rc.1".into()),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["dshVersion"], "0.1.5-rc.1");
        assert!(json.get("dsh_version").is_none());
        assert_eq!(json["version"], "1.0.4");
        assert_eq!(json["repo"], "wq3333/deepseek-harness-desktop");
    }

    #[test]
    fn setup_item_status_update() {
        let mut s = default_setup_state();
        set_setup_item(&mut s, "node", "ok", "v22.14.0");
        let node = s.items.iter().find(|i| i.key == "node").unwrap();
        assert_eq!(node.status, "ok");
        assert_eq!(node.detail, "v22.14.0");
        // unknown keys are ignored
        set_setup_item(&mut s, "nope", "failed", "x");
        assert_eq!(s.items.len(), 4);
    }

    #[test]
    fn parse_auth_url_with_token() {
        let text = "dsh web: http://127.0.0.1:3080/?token=i3HBr4uQ02tqKGThX5h0dGSzdqDEYxQODB5e8gAYmOc\n";
        assert_eq!(
            parse_auth_url(text),
            Some("http://127.0.0.1:3080/?token=i3HBr4uQ02tqKGThX5h0dGSzdqDEYxQODB5e8gAYmOc".into())
        );
    }

    #[test]
    fn parse_auth_url_with_lan_suffix() {
        let text = "dsh web: http://127.0.0.1:3080/?token=abc (LAN: http://192.168.1.5:3080/?token=abc)\n";
        assert_eq!(
            parse_auth_url(text),
            Some("http://127.0.0.1:3080/?token=abc".into())
        );
    }

    #[test]
    fn parse_auth_url_legacy_no_token() {
        let text = "dsh web: http://127.0.0.1:3080\n";
        assert_eq!(parse_auth_url(text), Some("http://127.0.0.1:3080".into()));
    }

    #[test]
    fn parse_auth_url_ignores_ansi_and_noise() {
        let text = "\u{1b}[90mSome npx noise\u{1b}[0m\n\u{1b}[36mdsh web:\u{1b}[0m \u{1b}[1mhttp://127.0.0.1:3080/?token=xyz\u{1b}[0m\n";
        assert_eq!(
            parse_auth_url(text),
            Some("http://127.0.0.1:3080/?token=xyz".into())
        );
    }

    #[test]
    fn parse_auth_url_missing() {
        assert_eq!(parse_auth_url(""), None);
        assert_eq!(parse_auth_url("just some output\nno marker here\n"), None);
        assert_eq!(parse_auth_url("dsh web: no url after marker\n"), None);
    }

    #[test]
    fn url_token_extraction() {
        assert_eq!(
            url_token("http://127.0.0.1:3080/?token=abc"),
            Some("abc".into())
        );
        assert_eq!(url_token("http://127.0.0.1:3080/"), None);
        assert_eq!(url_token("not a url"), None);
    }

    #[test]
    fn is_internal_url_accepts_dsh_server() {
        // Tokenized URL the harness webview is navigated to at startup.
        let u = url::Url::parse("http://127.0.0.1:3080/?token=abc").unwrap();
        assert!(is_internal_url(&u, 3080));
        // Same-origin SPA routes.
        let u = url::Url::parse("http://127.0.0.1:3080/some/route").unwrap();
        assert!(is_internal_url(&u, 3080));
        // localhost alias (dsh may redirect to it).
        let u = url::Url::parse("http://localhost:3080/?token=abc").unwrap();
        assert!(is_internal_url(&u, 3080));
    }

    #[test]
    fn is_internal_url_accepts_app_assets() {
        let u = url::Url::parse("http://tauri.localhost/loading.html").unwrap();
        assert!(is_internal_url(&u, 3080));
        let u = url::Url::parse("http://tauri.localhost/loading.html?mode=stopped").unwrap();
        assert!(is_internal_url(&u, 3080));
    }

    #[test]
    fn is_internal_url_rejects_external() {
        let u = url::Url::parse("https://github.com/wq3333/deepseek-harness-desktop").unwrap();
        assert!(!is_internal_url(&u, 3080));
        let u = url::Url::parse("https://chat.deepseek.com").unwrap();
        assert!(!is_internal_url(&u, 3080));
    }

    #[test]
    fn is_internal_url_rejects_wrong_port_or_host() {
        // Same local hosts, different port.
        let u = url::Url::parse("http://127.0.0.1:9999/x").unwrap();
        assert!(!is_internal_url(&u, 3080));
        let u = url::Url::parse("http://localhost:9999/x").unwrap();
        assert!(!is_internal_url(&u, 3080));
        // Loopback host on the right port, but not our server's.
        let u = url::Url::parse("http://192.168.1.5:3080/?token=abc").unwrap();
        assert!(!is_internal_url(&u, 3080));
    }

    #[test]
    fn is_internal_url_allows_non_http_schemes() {
        assert!(is_internal_url(&url::Url::parse("about:blank").unwrap(), 3080));
        assert!(is_internal_url(&url::Url::parse("data:text/plain,hi").unwrap(), 3080));
    }

    #[test]
    fn resolve_theme_maps_setting_and_os() {
        assert_eq!(resolve_theme("light", tauri::Theme::Dark), "light");
        assert_eq!(resolve_theme("dark", tauri::Theme::Light), "dark");
        assert_eq!(resolve_theme("system", tauri::Theme::Dark), "dark");
        assert_eq!(resolve_theme("system", tauri::Theme::Light), "light");
        // Unknown values fall back to following the OS.
        assert_eq!(resolve_theme("", tauri::Theme::Dark), "dark");
        assert_eq!(resolve_theme("banana", tauri::Theme::Light), "light");
    }

    #[test]
    fn theme_apply_js_sets_color_scheme_and_attribute() {
        let dark = theme_apply_js("dark");
        assert!(dark.contains("'dark'"));
        assert!(dark.contains("colorScheme"));
        assert!(dark.contains("data-dsh-theme"));
        let light = theme_apply_js("light");
        assert!(light.contains("'light'"));
    }

    #[test]
    fn theme_init_script_bakes_resolved_theme() {
        let init = theme_init_script("dark");
        // Waits for the root element (document-start) instead of touching
        // document.documentElement.style while it is still null.
        assert!(init.contains("requestAnimationFrame"));
        assert!(init.contains("colorScheme='dark'"));
        assert!(init.contains("data-dsh-theme','dark'"));
    }

    #[test]
    fn window_state_json_round_trip_and_defaults() {
        let state = WindowState {
            maximized: true,
            x: 120.0,
            y: 80.0,
            width: 1440.0,
            height: 900.0,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"maximized\":true"));
        let back: WindowState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.maximized, true);
        assert_eq!(back.x, 120.0);
        assert_eq!(back.width, 1440.0);
        // Missing fields fall back to defaults (forward/backward compat).
        let partial: WindowState = serde_json::from_str(r#"{"maximized":true}"#).unwrap();
        assert!(partial.maximized);
        assert_eq!(partial.width, 0.0);
    }

    #[test]
    fn settings_missing_theme_parses_as_system() {
        // Old settings.json (before the theme setting existed) must keep all
        // existing preferences and default the theme to "system".
        let s: Settings =
            serde_json::from_str(r#"{"closeStopsDsh":true,"autoUpdate":false}"#).unwrap();
        assert!(s.close_stops_dsh);
        assert!(!s.auto_update);
        assert_eq!(s.theme, "system");
    }

    #[test]
    fn rect_visible_on_monitors_matches_expected() {
        // Two monitors: primary 1920x1080 at (0,0), secondary 2560x1440 at (1920,0).
        let monitors = [(0i32, 0i32, 1920u32, 1080u32), (1920, 0, 2560, 1440)];
        // Fully inside the primary.
        assert!(rect_visible_on_monitors(200.0, 100.0, 1280.0, 720.0, 1.0, &monitors, 48.0));
        // Fully inside the secondary.
        assert!(rect_visible_on_monitors(3000.0, 200.0, 1280.0, 720.0, 1.0, &monitors, 48.0));
        // Off-screen to the left of everything.
        assert!(!rect_visible_on_monitors(-3000.0, 0.0, 1280.0, 720.0, 1.0, &monitors, 48.0));
        // Below every monitor.
        assert!(!rect_visible_on_monitors(0.0, 5000.0, 1280.0, 720.0, 1.0, &monitors, 48.0));
        // Straddling the seam between the two monitors still counts as visible.
        assert!(rect_visible_on_monitors(1800.0, 100.0, 1280.0, 720.0, 1.0, &monitors, 48.0));
        // A 1px sliver of overlap fails the margin requirement.
        assert!(!rect_visible_on_monitors(1919.5, 0.0, 1.0, 1.0, 1.0, &monitors, 48.0));
        // Different scale factors convert logical->physical first.
        assert!(rect_visible_on_monitors(100.0, 50.0, 1280.0, 720.0, 2.0, &monitors, 48.0));
    }
}
