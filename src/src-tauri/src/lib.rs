use std::fs::File;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{Emitter, Manager};

/// Default port used by the DSH web server. Override at runtime with the
/// `DSH_PORT` environment variable (useful to test without touching 3080).
const DEFAULT_PORT: u16 = 3080;

/// Official DeepSeek Chat web app (requires network).
const CHAT_URL: &str = "https://chat.deepseek.com";

/// GitHub repo used by the 关于 (About) dialog for update checks and updates.
const GITHUB_REPO: &str = "wq3333/deepseek-harness-desktop";
/// GitHub API endpoint for the latest published release.
const GITHUB_LATEST_API: &str =
    "https://api.github.com/repos/wq3333/deepseek-harness-desktop/releases/latest";

/// Height (logical px) of the custom title bar.
const TITLE_BAR_HEIGHT: f64 = 44.0;

/// Holds the child process spawned by this instance (if any), so it can be
/// cleaned up on exit.
struct ServerState(Mutex<Option<Child>>);

/// Current height (logical px) of the title bar webview: 44 normally, taller
/// while the "更多" dropdown (full window) or a toast (small) is showing.
struct BarHeight(Mutex<f64>);

/// Which content webview is currently visible ("harness" or "chat"). Used by
/// the F12 shortcut to open DevTools on the page the user is actually viewing.
struct CurrentTarget(Mutex<String>);

/// Live update/check state shared with the title bar UI. Written by the
/// check/update commands (possibly from background threads) and broadcast via
/// the `update-progress` event; the About dialog also queries it through
/// `get_update_state`, so closing and reopening the dialog keeps the status.
#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateState {
    /// idle | checking | downloading | installing | restarting | finalizing | done | error
    phase: String,
    /// 0..=100 when the progress is measurable; None = indeterminate bar.
    progress: Option<f64>,
    message: String,
    error: Option<String>,
    latest: Option<String>,
    update_available: bool,
    release_notes: String,
}

/// Latest update/check state, readable at any time via `get_update_state`.
struct SharedUpdateState(Mutex<UpdateState>);

/// True while a check/update is in flight (prevents starting another one).
fn update_active(state: &UpdateState) -> bool {
    matches!(
        state.phase.as_str(),
        "checking" | "downloading" | "installing" | "restarting" | "finalizing"
    )
}

/// Persist the update state and broadcast it to the title bar UI.
fn publish_update_state(app: &tauri::AppHandle, state: UpdateState) {
    *app.state::<SharedUpdateState>().0.lock().unwrap() = state.clone();
    let _ = app.emit("update-progress", state);
}

/// Return the current update/check state (the About dialog restores it when
/// reopened after being closed mid-update).
#[tauri::command]
fn get_update_state(app: tauri::AppHandle) -> UpdateState {
    app.state::<SharedUpdateState>().0.lock().unwrap().clone()
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

fn spawn_server(port: u16) -> std::io::Result<Child> {
    #[cfg(target_os = "windows")]
    {
        // CREATE_NO_WINDOW so no console window flashes next to the app.
        use std::os::windows::process::CommandExt;
        Command::new("cmd")
            .args(["/c", "npx", "@deepseek-ai/dsh", "web", "--port", &port.to_string()])
            .creation_flags(0x0800_0000)
            .spawn()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Command::new("npx")
            .args(["@deepseek-ai/dsh", "web", "--port", &port.to_string()])
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
    let Ok(inner) = window.inner_size() else { return };
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
        if matches!(event, tauri::WindowEvent::Resized(_)) {
            // Skip while minimized: Windows reports a ~19px height for the
            // minimized window and there is nothing to lay out until restore
            // (also avoids resizing webviews to a near-zero height).
            if window_for_layout.is_minimized().unwrap_or(true) {
                return;
            }
            let bar_height = app_handle_for_layout.state::<BarHeight>().0.lock().unwrap().clone();
            relayout(&window_for_layout, &bar, &harness, &chat, bar_height);
        }
    });
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = app_handle.exit(0);
        }
    });
}

/// Create the three child webviews of the single window: the persistent
/// title bar on top, and the two content webviews (harness / chat) that
/// toggle visibility below it. chat-content is added first and hidden right
/// away (it preloads in the background under harness-content).
fn add_webviews(
    window: tauri::Window,
    app_handle: tauri::AppHandle,
) -> tauri::Result<()> {
    // Size the webviews from the window's actual logical size instead of a
    // hard-coded 1280x800: the loading page is then centered at the final
    // size from its very first paint, avoiding a one-time "jump" when the
    // (maximized) window gets relaid out at startup.
    let scale = window.scale_factor().unwrap_or(1.0);
    let inner = window.inner_size().unwrap_or(tauri::PhysicalSize::new(1280, 800));
    let ls = inner.to_logical::<f64>(scale);
    let width = ls.width.max(1.0);
    let content_h = (ls.height - TITLE_BAR_HEIGHT).max(0.0);

    let chat = window.add_child(
        tauri::webview::WebviewBuilder::new(
            "chat-content",
            tauri::WebviewUrl::External(url::Url::parse(CHAT_URL).unwrap()),
        ),
        tauri::LogicalPosition::new(0.0, TITLE_BAR_HEIGHT),
        tauri::LogicalSize::new(width, content_h),
    )?;
    let _ = chat.hide();

    let harness = window.add_child(
        tauri::webview::WebviewBuilder::new(
            "harness-content",
            tauri::WebviewUrl::App("loading.html".into()),
        )
        // Match the loading page background so the webview never flashes white
        // before its first paint or during the loading -> dsh navigation
        // (WebView2 would otherwise show white). The window itself carries the
        // same background_color, so the pre-paint gap is seamless too.
        .background_color(tauri::window::Color(246, 248, 250, 255)),
        tauri::LogicalPosition::new(0.0, TITLE_BAR_HEIGHT),
        tauri::LogicalSize::new(width, content_h),
    )?;

    // Persistent title bar; added last so it is topmost in z-order.
    // Transparent so that when the "更多" dropdown expands it to the full
    // window height, the content webviews beneath remain visible (the title
    // bar strip itself keeps its own opaque background).
    let bar = window.add_child(
        tauri::webview::WebviewBuilder::new("bar", tauri::WebviewUrl::App("index.html".into()))
            .initialization_script("window.__DSH_TARGET__ = 'harness';")
            .transparent(true),
        tauri::LogicalPosition::new(0.0, 0.0),
        tauri::LogicalSize::new(width, TITLE_BAR_HEIGHT),
    )?;

    relayout(&window, &bar, &harness, &chat, TITLE_BAR_HEIGHT);
    attach_window_handlers(window, bar, harness, chat, app_handle);
    Ok(())
}

/// Resize the title bar webview to a given height. `height <= 0` means the
/// full window height (used by the "更多" dropdown); otherwise it is clamped
/// to `[TITLE_BAR_HEIGHT, window height]` (a small height is used for toasts).
#[tauri::command]
fn set_bar_height(app: tauri::AppHandle, height: f64) {
    let Some(bar) = app.get_webview("bar") else { return };
    let Some(window) = app.get_window("main") else { return };
    let Ok(inner) = window.inner_size() else { return };
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
    *app.state::<BarHeight>().0.lock().unwrap() = h;
    let _ = bar.set_position(tauri::LogicalPosition::new(0.0, 0.0));
    let _ = bar.set_size(tauri::LogicalSize::new(ls.width, h));
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

/// Exit the whole app without stopping the DSH server (title bar X button).
/// Closing the app leaves the spawned DSH server running (orphaned) so the
/// port keeps serving; use the "更多" -> "关闭 dsh 并退出" menu item to stop it.
#[tauri::command]
fn quit(app: tauri::AppHandle) {
    app.exit(0);
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
    let port = active_port();
    stop_dsh(&app, port);
    app.exit(0);
}

/// Restart the DSH server and refresh the harness view
/// ("更多" -> "重启 dsh 服务并刷新").
#[tauri::command]
fn restart_dsh(app: tauri::AppHandle) {
    let port = active_port();
    let url = server_url(port);
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
        if wait_for_port(port, Duration::from_secs(60)) {
            if let Ok(parsed) = url::Url::parse(&url) {
                if let Some(content) = app.get_webview("harness-content") {
                    let _ = content.navigate(parsed);
                }
            }
        } else {
            show_toast(&app, "dsh web server did not become ready on port {port}");
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
        let _ = content.navigate(
            url::Url::parse("http://tauri.localhost/loading.html?mode=stopped").unwrap(),
        );
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

/// Check for a dsh update and apply it if one is available
/// (更多 -> "更新 dsh"). All npm steps run hidden, in a background thread,
/// and the result is reported both through the shared update state (progress
/// bar in the About dialog) and with a toast.
#[tauri::command]
fn update_dsh(app: tauri::AppHandle) {
    {
        let state = app.state::<SharedUpdateState>().0.lock().unwrap().clone();
        if update_active(&state) {
            show_toast(&app, "已有更新正在进行");
            return;
        }
    }
    publish_update_state(
        &app,
        UpdateState {
            phase: "checking".into(),
            message: "正在检查 dsh 版本…".into(),
            ..Default::default()
        },
    );
    let port = active_port();
    let url = server_url(port);
    std::thread::spawn(move || match update_dsh_inner(&app, port, &url) {
        Ok(msg) => {
            publish_update_state(
                &app,
                UpdateState {
                    phase: "done".into(),
                    message: msg.clone(),
                    ..Default::default()
                },
            );
            show_toast(&app, msg);
        }
        Err(e) => {
            publish_update_state(
                &app,
                UpdateState {
                    phase: "error".into(),
                    error: Some(e.clone()),
                    message: format!("dsh 更新失败:{e}"),
                    ..Default::default()
                },
            );
            show_toast(&app, format!("dsh 更新失败:{e}"));
        }
    });
}

fn update_dsh_inner(app: &tauri::AppHandle, port: u16, url: &str) -> Result<String, String> {
    // npm on Windows is a .cmd shim, so run it through cmd (also resolves it
    // from PATH) with the hidden/no-console-window flag.
    let current = run_capture("cmd", &["/c", "npm", "ls", "-g", "@deepseek-ai/dsh", "--depth=0"])
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
    publish_update_state(
        app,
        UpdateState {
            phase: "installing".into(),
            progress: None,
            message: format!("正在安装 dsh v{latest}…"),
            latest: Some(latest.clone()),
            ..Default::default()
        },
    );
    stop_dsh(app, port);
    run_capture("cmd", &["/c", "npm", "install", "-g", "@deepseek-ai/dsh@latest"])
        .map_err(|e| format!("安装失败:{e}"))?;

    publish_update_state(
        app,
        UpdateState {
            phase: "restarting".into(),
            message: "正在重启 dsh 服务…".into(),
            latest: Some(latest.clone()),
            ..Default::default()
        },
    );
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
        if let Ok(parsed) = url::Url::parse(url) {
            if let Some(content) = app.get_webview("harness-content") {
                let _ = content.navigate(parsed);
            }
        }
    }

    Ok(match current {
        Some(c) => format!("dsh 已更新:v{c} → v{latest}，服务已重启"),
        None => format!("dsh 已安装 v{latest}，服务已启动"),
    })
}

// --- 关于 (About): app version + update check / update via GitHub releases ---

/// Minimal fields of the latest GitHub release that we need, extracted by the
/// PowerShell helper (so we never parse GitHub's full payload here).
struct LatestRelease {
    tag_name: String,
    body: Option<String>,
    asset_url: String,
}

/// Result of a manual update check, serialized back to the title bar UI.
#[derive(serde::Serialize)]
struct UpdateInfo {
    current: String,
    latest: String,
    update_available: bool,
    release_notes: String,
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

/// Desktop app info shown in the 关于 dialog: version + repo. `repo` comes
/// from the GITHUB_REPO constant so the UI never drifts from the backend.
#[derive(serde::Serialize)]
struct AppInfo {
    version: String,
    repo: String,
}

#[tauri::command]
fn get_app_info(app: tauri::AppHandle) -> AppInfo {
    AppInfo {
        version: app.package_info().version.to_string(),
        repo: GITHUB_REPO.to_string(),
    }
}

/// Open an external URL in the system default browser (used by the repo link
/// in the 关于 dialog). The bar webview itself never navigates away.
#[tauri::command]
fn open_url(url: String) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let quoted = format!("{url}");
        let _ = Command::new("cmd")
            .args(["/c", "start", "", &quoted])
            .creation_flags(0x0800_0000)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(&url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("xdg-open").arg(&url).spawn();
    }
}

/// Compare the running desktop version against the latest GitHub release.
/// Never throws for "no releases yet" (treated as up to date); network / API
/// errors come back as a Chinese error string. Progress is published through
/// the shared UpdateState so the dialog keeps its status across close/reopen.
#[tauri::command]
async fn check_update(app: tauri::AppHandle) -> Result<UpdateInfo, String> {
    let current = app.package_info().version.to_string();
    publish_update_state(
        &app,
        UpdateState {
            phase: "checking".into(),
            message: "正在检查更新…".into(),
            ..Default::default()
        },
    );
    let result = fetch_latest_release().map(|release| {
        let latest_raw = release.tag_name.trim().trim_start_matches('v').to_string();
        let update_available = matches!(
            (parse_semver(&latest_raw), parse_semver(&current)),
            (Some(l), Some(c)) if l != c
        );
        UpdateInfo {
            current,
            latest: latest_raw,
            update_available,
            release_notes: release.body.unwrap_or_default(),
            asset_url: release.asset_url,
        }
    });
    match result {
        Ok(info) => {
            publish_update_state(
                &app,
                UpdateState {
                    phase: "idle".into(),
                    message: if info.update_available {
                        format!("发现新版本 v{}", info.latest)
                    } else {
                        format!("已是最新版本(v{})", info.current)
                    },
                    latest: Some(info.latest.clone()),
                    update_available: info.update_available,
                    release_notes: info.release_notes.clone(),
                    ..Default::default()
                },
            );
            Ok(info)
        }
        Err(e) => {
            publish_update_state(
                &app,
                UpdateState {
                    phase: "error".into(),
                    error: Some(e.clone()),
                    message: format!("检查更新失败:{e}"),
                    ..Default::default()
                },
            );
            Err(e)
        }
    }
}

/// Portable-exe self update: download the new exe from the latest GitHub
/// release (hidden PowerShell, polled for real byte progress), then hand over
/// to a detached helper that waits for this process to exit, replaces the
/// running exe and relaunches it. Runs on a background thread so the About
/// dialog can be closed and reopened without losing the update status.
#[tauri::command]
fn update_app(app: tauri::AppHandle) -> Result<(), String> {
    {
        let state = app.state::<SharedUpdateState>().0.lock().unwrap().clone();
        if update_active(&state) {
            return Err("已有更新正在进行".into());
        }
    }
    std::thread::spawn(move || {
        if let Err(e) = update_app_inner(&app) {
            publish_update_state(
                &app,
                UpdateState {
                    phase: "error".into(),
                    error: Some(e.clone()),
                    message: format!("更新失败:{e}"),
                    ..Default::default()
                },
            );
        }
    });
    Ok(())
}

fn update_app_inner(app: &tauri::AppHandle) -> Result<(), String> {
    publish_update_state(
        app,
        UpdateState {
            phase: "checking".into(),
            message: "正在获取版本信息…".into(),
            ..Default::default()
        },
    );
    let release = fetch_latest_release()?;
    if release.asset_url.is_empty() {
        return Err("该发布中没有可用的 exe 更新包".into());
    }

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

    publish_update_state(
        app,
        UpdateState {
            phase: "downloading".into(),
            progress: Some(0.0),
            message: "正在下载更新包…".into(),
            ..Default::default()
        },
    );

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
                            publish_update_state(
                                app,
                                UpdateState {
                                    phase: "downloading".into(),
                                    progress: Some(pct),
                                    message: format!("正在下载更新包… {pct:.0}%"),
                                    ..Default::default()
                                },
                            );
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

    publish_update_state(
        app,
        UpdateState {
            phase: "finalizing".into(),
            progress: Some(100.0),
            message: "正在替换程序并重启…".into(),
            ..Default::default()
        },
    );

    // Write a detached helper that waits for us to exit, swaps the exe and
    // relaunches it. It keeps running after the parent (this app) exits, so it
    // can overwrite the file our own process no longer locks.
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
    app.exit(0);
    Ok(())
}

pub fn run() {
    let port = active_port();
    let url = server_url(port);

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
        .manage(SharedUpdateState(Mutex::new(UpdateState::default())))
        .invoke_handler(tauri::generate_handler![
            switch_to,
            quit,
            set_bar_height,
            stop_dsh_cmd,
            quit_with_dsh,
            restart_dsh,
            update_dsh,
            get_app_info,
            open_url,
            get_update_state,
            check_update,
            update_app
        ])
        .setup(move |app| {
            let app_handle = app.handle().clone();

            // --- single main window: persistent title bar + two content
            // webviews that are toggled by switch_to ---
            // No hide-then-show window + page-load-event + wait-thread
            // trickery: the window is born visible and maximized (tao applies
            // maximized before show, so there is no restore->maximize jump),
            // and its background_color matches the loading page + title bar
            // (#f6f8fa), so the pre-webview-paint gap shows the app color
            // instead of a white flash. The loading page stays centered at the
            // final size from its first frame (no startup "jitter").
            let window = tauri::window::WindowBuilder::new(app, "main")
                .title("DeepSeek Harness")
                .inner_size(1280.0, 800.0)
                .center()
                .maximized(true)
                .decorations(false)
                .resizable(true)
                .background_color(tauri::window::Color(246, 248, 250, 255))
                .build()?;
            add_webviews(window.clone(), app_handle.clone())?;
            // Defensive no-ops: the builder above already created the window
            // visible and maximized; these only matter if a future change ever
            // makes the window hidden at build time. The loading page keeps
            // its card hidden briefly (loading.html setTimeout) then fades it
            // in — no startup flash, and the loading page stays centered.
            let _ = window.maximize();
            let _ = window.show();

            // F12 toggles DevTools on the currently visible content webview.
            // Registration can fail if another app already owns F12; that
            // must not prevent the app from starting.
            use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut};
            if let Err(_e) = app.global_shortcut().register(Shortcut::new(None, Code::F12)) {
                //eprintln!("failed to register F12 global shortcut: {e}");
            }

            // --- spawn the DSH server if the port is free ---
            let started_by_us = !port_open(port);
            if started_by_us {
                match spawn_server(port) {
                    Ok(child) => {
                        *app.state::<ServerState>().0.lock().unwrap() = Some(child);
                    }
                    Err(e) => {
                        eprintln!("failed to spawn dsh web server: {e}");
                    }
                }
            }

            // Wait for the server (in the background so the loading page
            // renders immediately), then point the harness content at it.
            let url_for_thread = url.clone();
            std::thread::spawn(move || {
                if wait_for_port(port, Duration::from_secs(60)) {
                    if let Ok(parsed) = url::Url::parse(&url_for_thread) {
                        if let Some(content) = app_handle.get_webview("harness-content") {
                            let _ = content.navigate(parsed);
                        }
                    }
                } else {
                    eprintln!("dsh web server did not become ready on port {port}");
                }
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_, _| {});
}
