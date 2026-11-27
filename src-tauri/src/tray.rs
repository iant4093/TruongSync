//! System tray icon, native context menu, and main window show/focus helpers.

use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager,
};

pub fn show_main_window(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        let _ = app.show();
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub fn setup_system_tray(app: &AppHandle) -> tauri::Result<()> {
    let open_item = MenuItem::with_id(app, "open", "Open TruongSync", true, None::<&str>)?;
    let import_item = MenuItem::with_id(app, "import", "Import SD Card…", true, None::<&str>)?;
    let sync_item = MenuItem::with_id(
        app,
        "sync_cloud",
        "Sync Google Drive Now",
        true,
        None::<&str>,
    )?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit TruongSync", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[&open_item, &import_item, &sync_item, &sep, &quit_item],
    )?;

    let icon_bytes = include_bytes!("../icons/tray-icon.png");
    let icon = Image::from_bytes(icon_bytes)?;

    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut builder = TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .tooltip("TruongSync")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main_window(app),
            "import" => {
                show_main_window(app);
                let _ = app.emit("navigate-tab", "import");
            }
            "sync_cloud" => {
                show_main_window(app);
                let _ = app.emit("navigate-tab", "cloud");
                let app_clone = app.clone();
                tauri::async_runtime::spawn(async move {
                    crate::watcher::trigger_all_enabled_cloud_syncs(app_clone).await;
                });
            }
            "quit" => {
                let app_quit = app.clone();
                std::thread::spawn(move || {
                    if let Some(state) = app_quit.try_state::<crate::commands::AppState>() {
                        state.begin_shutdown();
                        state.cancel_all_and_wait(std::time::Duration::from_secs(2));
                    }
                    app_quit.exit(0);
                });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    #[cfg(target_os = "macos")]
    {
        builder = builder.icon_as_template(true);
    }

    builder.build(app)?;
    Ok(())
}
