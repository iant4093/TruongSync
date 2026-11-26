pub mod auto_launch;
pub mod commands;
pub mod power;
pub mod tray;
pub mod watcher;

use tauri::{Emitter, Manager};
use truongsync_db::models::StoragePaths;
use truongsync_db::DbHandle;

const AUTOSTART_ARG: &str = "--autostart";

fn setup_app(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "truongsync_desktop_lib=info".into()),
        )
        .try_init();

    let paths = StoragePaths::from_data_dir(StoragePaths::default_data_dir());
    paths.ensure_dirs()?;
    let db = DbHandle::open(&paths.db_path)?;

    let initial_vol = std::env::args()
        .skip_while(|a| a != "--import-volume")
        .nth(1)
        .map(|v| auto_launch::normalize_import_volume_arg(&v))
        .filter(|v| !v.is_empty());

    let state = commands::AppState::new(db, paths, initial_vol);
    app.manage(state);

    let autostart_launch = std::env::args().any(|arg| arg == AUTOSTART_ARG);
    let main_url = tauri::WebviewUrl::App("index.html".into());

    let builder = tauri::WebviewWindowBuilder::new(app, "main", main_url)
        .title("TruongSync")
        .visible(false)
        .inner_size(1220.0, 800.0)
        .min_inner_size(940.0, 620.0)
        .decorations(!cfg!(target_os = "windows"))
        .transparent(cfg!(target_os = "windows"))
        .shadow(true)
        .center()
        .theme(Some(tauri::Theme::Dark));

    #[cfg(target_os = "macos")]
    let builder = builder
        .background_color(tauri::window::Color(0x20, 0x20, 0x20, 0xFF))
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(20.0, 18.0));

    let window = builder.build()?;
    let main_win = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            #[cfg(target_os = "macos")]
            if main_win.is_fullscreen().unwrap_or(false) {
                let _ = main_win.set_fullscreen(false);
            }
            let _ = main_win.hide();
        }
    });

    if autostart_launch {
        let _ = window.hide();
    } else {
        tray::show_main_window(app.handle());
    }

    tray::setup_system_tray(app.handle())?;
    watcher::start_volume_watcher(app.handle().clone());
    watcher::start_background_scheduler(app.handle().clone());

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Fast headless exit path for macOS LaunchAgent `--check-mounts`:
    // If a new SD card arrived, launch the main app bundle via `--import-volume` and exit immediately
    // so the LaunchAgent never owns the GUI process (and warm handoff uses `--import-volume`).
    if std::env::args().any(|a| a == "--check-mounts") {
        if let Some(new_mount) = auto_launch::check_new_sd_card_for_launch_agent() {
            auto_launch::launch_gui_for_volume(&new_mount);
        }
        return;
    }

    let context = tauri::generate_context!();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if let Some(pos) = args.iter().position(|a| a == "--import-volume") {
                if let Some(vol_raw) = args.get(pos + 1) {
                    let vol = auto_launch::normalize_import_volume_arg(vol_raw);
                    if !vol.is_empty() {
                        tray::show_main_window(app);
                        let _ = app.emit("navigate-tab", "import");
                        let _ = app.emit("auto-import-volume-path", vol);
                        return;
                    }
                }
            }
            if !args.iter().any(|a| a == AUTOSTART_ARG) {
                tray::show_main_window(app);
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_ARG]),
        ))
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(setup_app)
        .invoke_handler(tauri::generate_handler![
            commands::get_bootstrap_info,
            commands::check_tools,
            commands::get_settings,
            commands::save_settings,
            commands::set_os_native_auto_launch,
            commands::list_volumes,
            commands::eject_volume_cmd,
            commands::list_year_categories,
            commands::create_category_folder,
            commands::list_import_presets,
            commands::save_import_preset,
            commands::delete_import_preset,
            commands::scan_card,
            commands::preflight_import,
            commands::preview_import_plan,
            commands::run_import_pipeline,
            commands::cancel_active_job,
            commands::find_matching_gpx,
            commands::preview_geotag,
            commands::run_geotag,
            commands::list_cloud_profiles,
            commands::save_cloud_profile,
            commands::delete_cloud_profile,
            commands::import_rclone_conf,
            commands::list_rclone_remotes_cmd,
            commands::connect_google_drive,
            commands::verify_cloud_cmd,
            commands::sync_cloud_cmd,
            commands::list_external_profiles,
            commands::save_external_profile,
            commands::delete_external_profile,
            commands::verify_external_cmd,
            commands::sync_external_cmd,
            commands::list_jobs,
            commands::list_job_items,
            commands::show_main_window_cmd,
            commands::quit_app_cmd,
            commands::check_update_feed,
            commands::install_native_update,
        ])
        .build(context)
        .expect("error while building TruongSync");

    app.run(|app_handle, event| {
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen {
            has_visible_windows,
            ..
        } = event
        {
            if !has_visible_windows {
                tray::show_main_window(app_handle);
            }
        }
        let _ = (app_handle, event);
    });
}
