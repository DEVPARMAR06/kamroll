// Kamroll desktop app.
//
// The engine is part of this program. The window loads its pages through a private `kamroll` scheme that is
// answered in memory, so there is no web server, no port, and no address that a browser or another program
// could open. Closing the window ends everything; an interrupted copy resumes on the next backup.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::sync::Arc;
use tauri::{LogicalSize, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder};

/// Size the window to the screen it opens on, so no part of it hides under the taskbar or the Dock.
/// Small screens get a maximised window; larger ones a comfortable size, centred in the usable area.
fn fit_to_screen(window: &tauri::WebviewWindow) {
    let monitor = window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten());
    let Some(m) = monitor else { return };
    let scale = m.scale_factor();
    let area = m.work_area();
    let (w, h) = (area.size.width as f64 / scale, area.size.height as f64 / scale);
    if w < 1100.0 || h < 700.0 {
        let _ = window.maximize();
        return;
    }
    // Leave room for the title bar (about 40 points) and a margin around the window.
    let width = (w * 0.92).min(1360.0);
    let height = (h * 0.92 - 40.0).min(900.0);
    let _ = window.set_size(LogicalSize::new(width, height));
    if let Ok(outer) = window.outer_size() {
        let x = area.position.x + (area.size.width as i32 - outer.width as i32) / 2;
        let y = area.position.y + (area.size.height as i32 - outer.height as i32) / 2;
        let _ = window.set_position(PhysicalPosition::new(x.max(area.position.x), y.max(area.position.y)));
    }
}

fn start_page() -> String {
    // Windows web views expose a custom scheme as http://<scheme>.localhost; macOS and Linux use <scheme>://.
    let (base, platform) = if cfg!(windows) {
        ("http://kamroll.localhost/", "win")
    } else if cfg!(target_os = "macos") {
        ("kamroll://localhost/", "mac")
    } else {
        ("kamroll://localhost/", "linux")
    };
    format!("{base}?desktop={platform}")
}

fn main() {
    let engine = Arc::new(kamroll_engine::Embedded::new());
    tauri::Builder::default()
        // A second launch focuses the first window instead of starting a second engine on the same backup.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .register_asynchronous_uri_scheme_protocol("kamroll", move |_context, request, responder| {
            let engine = engine.clone();
            tauri::async_runtime::spawn(async move { responder.respond(engine.handle(request).await) });
        })
        .setup(|app| {
            // macOS needs a menu bar for Cut, Copy, Paste and the usual shortcuts to work in text fields.
            #[cfg(target_os = "macos")]
            app.set_menu(tauri::menu::Menu::default(app.handle())?)?;

            let page = WebviewUrl::CustomProtocol(start_page().parse().expect("the start page address is valid"));
            let builder = WebviewWindowBuilder::new(app, "main", page)
                .title("Kamroll")
                .inner_size(1180.0, 760.0)
                .min_inner_size(820.0, 560.0)
                .visible(false);
            // On macOS the web page draws under the title bar, with the traffic lights floating over it.
            #[cfg(target_os = "macos")]
            let builder = builder.title_bar_style(tauri::TitleBarStyle::Overlay).hidden_title(true);
            let window = builder.build()?;
            fit_to_screen(&window);
            let _ = window.show();
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Kamroll");
}
