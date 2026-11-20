#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use desktop_manager::{model::*, Manager};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tauri::{Emitter, Manager as _, State};
use tauri_plugin_dialog::{
    DialogExt, MessageDialogButtons, MessageDialogKind, MessageDialogResult,
};
type AppManager = Arc<Manager>;
async fn work<T: Send + 'static>(
    manager: AppManager,
    f: impl FnOnce(&Manager) -> std::result::Result<T, desktop_manager::Error> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || f(&manager).map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())?
}
// Keep the public command surface explicit and small. All blocking work leaves the UI thread.
#[tauri::command]
async fn snapshot(state: State<'_, AppManager>) -> Result<Snapshot, String> {
    work(state.inner().clone(), |m| Ok(m.snapshot())).await
}
#[tauri::command]
async fn save_workspace(
    state: State<'_, AppManager>,
    workspace: Workspace,
    secrets: Option<Secrets>,
) -> Result<Workspace, String> {
    work(state.inner().clone(), move |m| {
        let existing = m
            .snapshot()
            .workspaces
            .into_iter()
            .find(|saved| saved.id == workspace.id);
        if existing.as_ref().is_none_or(|saved| {
            saved.path != workspace.path || saved.core_command != workspace.core_command
        }) {
            desktop_manager::core::validate_override(
                &workspace.core_command,
                std::path::Path::new(&workspace.path),
            )?;
        }
        m.save_workspace(workspace, secrets)
    })
    .await
}
#[tauri::command]
async fn delete_workspace(state: State<'_, AppManager>, id: String) -> Result<(), String> {
    work(state.inner().clone(), move |m| m.delete_workspace(&id)).await
}
macro_rules! operation {
    ($name:ident,$method:ident,$result:ty) => {
        #[tauri::command]
        async fn $name(state: State<'_, AppManager>, id: String) -> Result<$result, String> {
            work(state.inner().clone(), move |m| m.$method(&id)).await
        }
    };
}
operation!(stop_workspace, stop, Status);
operation!(retry_tunnel, retry_tunnel, Status);
operation!(activity, activity, Vec<Activity>);
fn guard_workspace_core(m: &Manager, id: &str) -> Result<(), desktop_manager::Error> {
    if let Some(workspace) = m.snapshot().workspaces.into_iter().find(|w| w.id == id) {
        desktop_manager::core::validate_override(
            &workspace.core_command,
            std::path::Path::new(&workspace.path),
        )?;
    }
    Ok(())
}
macro_rules! core_operation {
    ($name:ident,$method:ident,$result:ty) => {
        #[tauri::command]
        async fn $name(state: State<'_, AppManager>, id: String) -> Result<$result, String> {
            work(state.inner().clone(), move |m| {
                guard_workspace_core(m, &id)?;
                m.$method(&id)
            })
            .await
        }
    };
}
core_operation!(start_workspace, start, Status);
core_operation!(restart_workspace, restart, Status);
core_operation!(diagnose, diagnose, Vec<Diagnostic>);
core_operation!(export_diagnostics, export_diagnostics, String);

#[tauri::command]
async fn copy_text(app: tauri::AppHandle, text: String) -> Result<bool, String> {
    #[cfg(target_os = "linux")]
    {
        if text.len() > 1024 * 1024 || text.contains('\0') {
            return Err("Clipboard text is not supported".into());
        }
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        app.run_on_main_thread(move || {
            let result = gtk::gdk::Display::default()
                .ok_or_else(|| "The system clipboard is unavailable".to_owned())
                .map(|display| {
                    gtk::Clipboard::for_display(&display, &gtk::gdk::SELECTION_CLIPBOARD)
                        .set_text(&text);
                    true
                });
            let _ = send.send(result);
        })
        .map_err(|error| error.to_string())?;
        tauri::async_runtime::spawn_blocking(move || {
            receive
                .recv_timeout(std::time::Duration::from_secs(3))
                .map_err(|_| "The system clipboard is unavailable".to_owned())?
        })
        .await
        .map_err(|error| error.to_string())?
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (app, text);
        Ok(false)
    }
}
operation!(check_public_route, check_public_route, String);
operation!(auth_details, auth_details, serde_json::Value);
#[tauri::command]
async fn logs(
    state: State<'_, AppManager>,
    id: String,
    kind: String,
    cursor: u64,
) -> Result<Logs, String> {
    work(state.inner().clone(), move |m| m.logs(&id, &kind, cursor)).await
}
#[tauri::command]
async fn connection_config(
    state: State<'_, AppManager>,
    id: String,
    public: bool,
) -> Result<String, String> {
    work(state.inner().clone(), move |m| {
        m.connection_config(&id, public)
    })
    .await
}
struct TrayItems {
    show: tauri::menu::MenuItem<tauri::Wry>,
    quit: tauri::menu::MenuItem<tauri::Wry>,
}

fn tray_labels(language: &str) -> (&'static str, &'static str) {
    if language == "zh" {
        ("显示 Coding Tools MCP", "停止服务并退出")
    } else {
        ("Show Coding Tools MCP", "Stop services and quit")
    }
}

#[tauri::command]
async fn save_settings(
    app: tauri::AppHandle,
    state: State<'_, AppManager>,
    settings: Settings,
) -> Result<Settings, String> {
    let saved = work(state.inner().clone(), move |m| m.save_settings(settings)).await?;
    if let Some(items) = app.try_state::<TrayItems>() {
        let labels = tray_labels(&saved.language);
        let _ = items.show.set_text(labels.0);
        let _ = items.quit.set_text(labels.1);
    }
    Ok(saved)
}
#[tauri::command]
async fn install_core(state: State<'_, AppManager>, version: String) -> Result<String, String> {
    work(state.inner().clone(), move |m| m.install_core(&version)).await
}
#[tauri::command]
async fn rollback_core(state: State<'_, AppManager>) -> Result<String, String> {
    work(state.inner().clone(), |m| m.rollback_core()).await
}
#[tauri::command]
async fn open_install_logs(state: State<'_, AppManager>) -> Result<(), String> {
    let folder = state.storage.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        open::that_detached(folder).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}
#[tauri::command]
async fn open_setup_help(topic: String) -> Result<(), String> {
    let url = match topic.as_str() {
        "core" => "https://github.com/xyTom/coding-tools-mcp/releases",
        "cloudflare" => "https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/downloads/",
        _ => return Err("Unknown setup help topic".into()),
    };
    tauri::async_runtime::spawn_blocking(move || {
        open::that_detached(url).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}
#[tauri::command]
async fn cloudflare_login(state: State<'_, AppManager>) -> Result<String, String> {
    work(state.inner().clone(), |m| m.cloudflare_login()).await
}
#[tauri::command]
async fn setup_named_tunnel(
    state: State<'_, AppManager>,
    id: String,
    name: String,
    hostname: String,
) -> Result<Workspace, String> {
    work(state.inner().clone(), move |m| {
        m.setup_named_tunnel(&id, &name, &hostname)
    })
    .await
}
#[tauri::command]
async fn validate_workspace_path(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        desktop_manager::runtime::validate_workspace_folder(&path).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn validate_core_override(path: String, args: Vec<String>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        desktop_manager::core::validate_override(&args, std::path::Path::new(&path))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn pick_directory(app: tauri::AppHandle) -> Result<Option<String>, String> {
    pick_path(app, true).await
}
#[tauri::command]
async fn pick_core_executable(app: tauri::AppHandle) -> Result<Option<String>, String> {
    pick_path(app, false).await
}
async fn pick_path(app: tauri::AppHandle, directory: bool) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let picker = app.dialog().file();
        let selection = if directory {
            picker.blocking_pick_folder()
        } else {
            picker.blocking_pick_file()
        };
        selection
            .map(|f| {
                f.into_path().map_err(|e| e.to_string()).and_then(|p| {
                    p.into_os_string()
                        .into_string()
                        .map_err(|_| "Folder path must use valid UTF-8 characters".to_owned())
                })
            })
            .transpose()
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn open_workspace(state: State<'_, AppManager>, id: String) -> Result<(), String> {
    let path = state.workspace_path(&id).map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn_blocking(move || open::that(path).map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn quit_app(app: tauri::AppHandle, state: State<'_, AppManager>) -> Result<(), String> {
    work(state.inner().clone(), |m| m.shutdown()).await?;
    app.state::<AtomicBool>().store(true, Ordering::SeqCst);
    app.exit(0);
    Ok(())
}
fn show(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}
fn request_quit(app: tauri::AppHandle) {
    let m = app.state::<AppManager>().inner().clone();
    tauri::async_runtime::spawn(async move {
        match work(m, |m| m.shutdown()).await {
            Ok(()) => {
                app.state::<AtomicBool>().store(true, Ordering::SeqCst);
                app.exit(0);
            }
            Err(error) => {
                show(&app);
                let _ = app.emit("shutdown-error", &error);
                let zh = app.state::<AppManager>().settings().language == "zh";
                app.dialog()
                    .message(error)
                    .title(tr(zh, "Could not stop services", "无法停止服务"))
                    .kind(tauri_plugin_dialog::MessageDialogKind::Error)
                    .show(|_| {});
            }
        }
    });
}
/// Native dialogs appear before, or without, the web UI and its translations.
fn tr(zh: bool, en: &'static str, chinese: &'static str) -> &'static str {
    if zh {
        chinese
    } else {
        en
    }
}
fn startup_reason(zh: bool, error: &desktop_manager::Error) -> String {
    let text = format!("{error:#}");
    if !zh {
        return text;
    }
    let known = [
        (
            desktop_manager::storage::LOCKED,
            "另一个桌面管理器正在使用此配置",
        ),
        (
            "Configuration is invalid; original files were preserved",
            "配置文件无效；原始文件已保留",
        ),
        (
            "The main desktop configuration is missing; restore it from backup",
            "主配置文件缺失；请从备份恢复",
        ),
        ("Could not read desktop configuration", "无法读取桌面配置"),
        ("Configuration is not valid JSON", "配置不是有效的 JSON"),
        (
            "Unsupported configuration version; use a compatible desktop release",
            "不支持的配置版本；请使用兼容的桌面版本",
        ),
    ];
    known
        .iter()
        .fold(text, |text, (en, chinese)| text.replace(en, chinese))
}
fn startup_error(app: &tauri::App, error: desktop_manager::Error) {
    use desktop_manager::storage::{Storage, CONFIG_BACKUP, CONFIG_FILE, LOCKED};
    app.state::<AtomicBool>().store(true, Ordering::SeqCst);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
    let handle = app.handle().clone();
    let home = Manager::default_home().ok();
    let zh = home
        .as_ref()
        .is_some_and(|dir| Storage::preferred_language(dir) == "zh");
    if format!("{error:#}").contains(LOCKED) {
        // The running instance owns the configuration; nothing is damaged.
        app.dialog()
            .message(tr(
                zh,
                "Coding Tools MCP is already running. Open it from the system tray icon.",
                "Coding Tools MCP 已在运行。请通过系统托盘图标打开。",
            ))
            .title(tr(
                zh,
                "Coding Tools MCP is already running",
                "Coding Tools MCP 已在运行",
            ))
            .kind(MessageDialogKind::Info)
            .show(move |_| handle.exit(0));
        return;
    }
    let preview = home
        .as_ref()
        .and_then(|dir| Storage::open(dir.clone()).ok()?.backup_preview());
    let location = home.as_ref().map_or(String::new(), |dir| {
        format!(
            "\n\n{}: {}\n{}: {}",
            tr(zh, "Configuration", "配置文件"),
            dir.join(CONFIG_FILE).display(),
            tr(zh, "Backup", "备份"),
            dir.join(CONFIG_BACKUP).display()
        )
    });
    let detail = preview.map_or_else(
        || tr(zh, "No usable backup was found. Open the configuration folder to repair the files, or start fresh with an empty configuration. Damaged files are kept separately; nothing is deleted.", "未找到可用的备份。可以打开配置文件夹修复文件，或使用空白配置重新开始。损坏的文件会另行保留，不会被删除。").to_owned(),
        |(time, count)| {
            let local: chrono::DateTime<chrono::Local> = time.into();
            let saved = local.format("%Y-%m-%d %H:%M:%S %Z");
            if zh {
                format!("备份保存于 {saved}；包含工作区：{count} 个。此备份之后所做的更改可能会丢失。损坏的文件会另行保留。")
            } else {
                format!("Backup saved {saved}; workspaces included: {count}. Changes made after this backup may be missing. The damaged file will be kept separately.")
            }
        },
    );
    let close = tr(zh, "Close", "关闭");
    let dialog = app
        .dialog()
        .message(format!(
            "{}{location}\n\n{detail}",
            startup_reason(zh, &error)
        ))
        .title(tr(
            zh,
            "Coding Tools MCP could not start",
            "Coding Tools MCP 无法启动",
        ))
        .kind(MessageDialogKind::Error);
    // Replace the unreadable files only on request, then start again.
    let recover =
        move |handle: tauri::AppHandle,
              failure: &'static str,
              action: fn(&Storage) -> Result<(), desktop_manager::Error>| {
            tauri::async_runtime::spawn_blocking(move || {
                let result = Manager::default_home()
                    .and_then(Storage::open)
                    .and_then(|s| action(&s));
                match result {
                    Ok(()) => handle.request_restart(),
                    Err(error) => {
                        let app = handle.clone();
                        handle
                            .dialog()
                            .message(format!("{failure}: {error:#}"))
                            .kind(MessageDialogKind::Error)
                            .show(move |_| app.exit(1));
                    }
                }
            });
        };
    if preview.is_none() {
        if let Some(dir) = home.filter(|dir| dir.is_dir()) {
            let open_folder = tr(zh, "Open configuration folder", "打开配置文件夹");
            let fresh = tr(zh, "Start fresh", "重新开始");
            dialog
                .buttons(MessageDialogButtons::YesNoCancelCustom(
                    open_folder.into(),
                    fresh.into(),
                    close.into(),
                ))
                .show_with_result(move |choice| match choice {
                    MessageDialogResult::Custom(label) if label == fresh => recover(
                        handle,
                        tr(zh, "Could not start fresh", "无法重新开始"),
                        Storage::start_fresh,
                    ),
                    MessageDialogResult::No => recover(
                        handle,
                        tr(zh, "Could not start fresh", "无法重新开始"),
                        Storage::start_fresh,
                    ),
                    MessageDialogResult::Custom(label) if label == open_folder => {
                        let _ = open::that_detached(dir);
                        handle.exit(1);
                    }
                    MessageDialogResult::Yes => {
                        let _ = open::that_detached(dir);
                        handle.exit(1);
                    }
                    _ => handle.exit(1),
                });
        } else {
            dialog.show(move |_| handle.exit(1));
        }
        return;
    }
    dialog
        .buttons(MessageDialogButtons::OkCancelCustom(
            tr(zh, "Restore backup", "恢复备份").into(),
            close.into(),
        ))
        .show(move |restore| {
            if !restore {
                handle.exit(1);
                return;
            }
            recover(
                handle,
                tr(zh, "Could not restore the backup", "无法恢复备份"),
                Storage::restore_backup,
            );
        });
}

/// The single-instance plugin needs a session bus on Linux. Without one, a
/// second launch reaches the configuration lock instead; ask the owner of
/// that lock to show its window over a private socket in the same folder.
#[cfg(unix)]
const INSTANCE_SOCKET: &str = "manager.sock";
#[cfg(unix)]
fn raise_running_instance() -> bool {
    use desktop_manager::storage::{Storage, LOCKED};
    let Ok(home) = Manager::default_home() else {
        return false;
    };
    match Storage::open(home.clone()) {
        Err(error) if format!("{error:#}").contains(LOCKED) => {
            std::os::unix::net::UnixStream::connect(home.join(INSTANCE_SOCKET)).is_ok()
        }
        _ => false,
    }
}
#[cfg(unix)]
fn listen_for_second_instance(app: tauri::AppHandle) {
    let Ok(home) = Manager::default_home() else {
        return;
    };
    // This process holds the configuration lock, so any socket is stale.
    let path = home.join(INSTANCE_SOCKET);
    let _ = std::fs::remove_file(&path);
    if let Ok(listener) = std::os::unix::net::UnixListener::bind(path) {
        std::thread::spawn(move || {
            for _ in listener.incoming().flatten() {
                show(&app);
            }
        });
    }
}

/// Tauri reports a missing WebView2 Runtime from inside window creation and
/// then leaves a windowless single-instance process behind. Check first and
/// exit as soon as the message is dismissed.
#[cfg(windows)]
fn require_webview2() {
    if tauri::webview_version().is_ok() {
        return;
    }
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Coding Tools MCP")
        .set_description(
            "Could not find the Microsoft Edge WebView2 Runtime.\n\nInstall it from https://developer.microsoft.com/microsoft-edge/webview2 and start Coding Tools MCP again.\n\n找不到 Microsoft Edge WebView2 Runtime。\n\n请从 https://developer.microsoft.com/microsoft-edge/webview2 安装后重新启动 Coding Tools MCP。",
        )
        .show();
    std::process::exit(1);
}

fn main() {
    if let Some(code) = desktop_manager::supervisor::run_if_requested() {
        std::process::exit(code);
    }
    #[cfg(windows)]
    require_webview2();
    #[cfg(unix)]
    if raise_running_instance() {
        return;
    }
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if app.try_state::<AppManager>().is_some() {
                show(app);
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(AtomicBool::new(false))
        .setup(|app| {
            let manager = match Manager::default_home().and_then(|home| {
                #[cfg(unix)]
                let supervisor = Some(std::env::current_exe()?);
                #[cfg(not(unix))]
                let supervisor = None;

                // Only activate the sidecar in builds with its packaging overlay.
                // Use Tauri's actual executable location (including AppImage) and
                // the specific packaged binary name, not the working directory.
                let bundled_uv =
                    if app
                        .config()
                        .bundle
                        .external_bin
                        .as_ref()
                        .is_some_and(|binaries| {
                            binaries
                                .iter()
                                .any(|path| path == "gen/runtime-installer/coding-tools-mcp-uv")
                        })
                    {
                        let executable = tauri::utils::platform::current_exe()?;
                        let name = if cfg!(windows) {
                            "coding-tools-mcp-uv.exe"
                        } else {
                            "coding-tools-mcp-uv"
                        };
                        Some(executable.with_file_name(name))
                    } else {
                        None
                    };
                Manager::open_native(home, supervisor, bundled_uv)
            }) {
                Ok(manager) => manager,
                Err(error) => {
                    startup_error(app, error);
                    return Ok(());
                }
            };
            let labels = tray_labels(&manager.settings().language);
            app.manage(manager);
            #[cfg(unix)]
            listen_for_second_instance(app.handle().clone());
            let show_item =
                tauri::menu::MenuItem::with_id(app, "show", labels.0, true, None::<&str>)?;
            let quit_item =
                tauri::menu::MenuItem::with_id(app, "quit", labels.1, true, None::<&str>)?;
            let menu = tauri::menu::Menu::with_items(app, &[&show_item, &quit_item])?;
            app.manage(TrayItems {
                show: show_item,
                quit: quit_item,
            });
            let mut tray = tauri::tray::TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("Coding Tools MCP")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show(app),
                    "quit" => request_quit(app.clone()),
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                if !app.state::<AtomicBool>().load(Ordering::SeqCst) {
                    api.prevent_close();
                    let manager = app.state::<AppManager>();
                    let settings = manager.settings();
                    if settings.close_to_tray {
                        let _ = window.hide();
                        if manager.claim_tray_hint() {
                            let zh = settings.language == "zh";
                            app.dialog()
                                .message(tr(
                                    zh,
                                    "Coding Tools MCP is still running in the system tray, and your services stay available. Use the tray icon to show the window or to stop services and quit. This message is shown only once.",
                                    "Coding Tools MCP 仍在系统托盘中运行，服务保持可用。可通过托盘图标重新显示窗口，或停止服务并退出。此提示只显示一次。",
                                ))
                                .title("Coding Tools MCP")
                                .kind(MessageDialogKind::Info)
                                .show(|_| {});
                        }
                    } else {
                        request_quit(app.clone());
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            save_workspace,
            delete_workspace,
            start_workspace,
            stop_workspace,
            restart_workspace,
            retry_tunnel,
            activity,
            logs,
            diagnose,
            check_public_route,
            export_diagnostics,
            connection_config,
            copy_text,
            auth_details,
            save_settings,
            pick_directory,
            pick_core_executable,
            validate_workspace_path,
            validate_core_override,
            open_workspace,
            install_core,
            open_install_logs,
            open_setup_help,
            rollback_core,
            cloudflare_login,
            setup_named_tunnel,
            quit_app
        ])
        .build(tauri::generate_context!())
        .expect("Could not start Coding Tools MCP desktop");
    app.run(|app, event| {
        #[cfg(target_os = "macos")]
        if matches!(&event, tauri::RunEvent::Reopen { .. })
            && app.try_state::<AppManager>().is_some()
        {
            show(app);
        }
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            if !app.state::<AtomicBool>().load(Ordering::SeqCst) {
                api.prevent_exit();
                request_quit(app.clone());
            }
        }
    });
}
