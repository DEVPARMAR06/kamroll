// Kamroll desktop shell.
//
// It does three things: start the local engine (`kamroll-engine ui`) on a free
// port with a random access token, open a window on it once it answers, and stop the engine when
// the window closes. All the app logic stays in the engine, so the same window will later host a
// Rust engine without any change to the page.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{LogicalSize, Manager, PhysicalPosition, RunEvent, WebviewUrl, WebviewWindowBuilder};

struct Engine(Mutex<Option<Child>>);

fn free_port() -> std::io::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// A random token from the operating system's hash seeds. It only guards a loopback port.
fn random_token() -> String {
    (0..3)
        .map(|_| format!("{:016x}", RandomState::new().build_hasher().finish()))
        .collect()
}

#[cfg(debug_assertions)]
fn project_root() -> PathBuf {
    // desktop/src-tauri -> project root, so a development run finds the same config.toml as the CLI.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Where to find the engine: $KAMROLL_ENGINE (or $PHOTOVAULT_ENGINE), a development build of the Rust engine,
/// the bundled copy in the installed app, a copy next to this program, then `kamroll-engine` on the PATH.
fn engine_program(resources: Option<PathBuf>) -> PathBuf {
    for var in ["KAMROLL_ENGINE", "PHOTOVAULT_ENGINE"] {
        if let Ok(p) = std::env::var(var) {
            return PathBuf::from(p);
        }
    }
    let name = if cfg!(windows) { "kamroll-engine.exe" } else { "kamroll-engine" };
    #[cfg(debug_assertions)]
    {
        for profile in ["debug", "release"] {
            let candidate = project_root().join("engine").join("target").join(profile).join(name);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    // An installed app carries the engine as a folder of resources.
    if let Some(dir) = resources {
        let candidate = dir.join("engine").join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.join(name).exists() {
                return dir.join(name);
            }
        }
    }
    PathBuf::from("kamroll-engine")
}

fn start_engine(resources: Option<PathBuf>) -> Result<(Child, u16, String), String> {
    let port = free_port().map_err(|e| format!("No free local port: {e}"))?;
    let token = random_token();
    let program = engine_program(resources);
    let mut cmd = Command::new(&program);
    cmd.args(["ui", "--no-open", "--port", &port.to_string(), "--token", &token])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(debug_assertions)]
    cmd.current_dir(project_root());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flashes up
    }
    let child = cmd.spawn().map_err(|e| {
        format!(
            "Could not start the engine ({}): {e}. Build it with `cargo build` in the engine folder, \
             or set KAMROLL_ENGINE to the kamroll-engine program.",
            program.display()
        )
    })?;
    Ok((child, port, token))
}

fn engine_ready(port: u16, token: &str) -> bool {
    let addr: SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let req = format!("GET /api/health?token={token} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.starts_with("HTTP/1.1 200")
}

fn kill_engine(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<Engine>() {
        if let Ok(mut guard) = state.0.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn show_error(window: &tauri::WebviewWindow, message: &str) {
    let js = format!("window.showError({})", serde_json::to_string(message).unwrap_or_else(|_| "\"error\"".into()));
    let _ = window.eval(&js);
}

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

fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(windows) {
        "win"
    } else {
        "linux"
    }
}

fn main() {
    let app = tauri::Builder::default()
        // A second launch focuses the first window instead of starting a second engine on the same backup.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            app.manage(Engine(Mutex::new(None)));
            // macOS needs a menu bar for Cut, Copy, Paste and the usual shortcuts to work in text fields.
            #[cfg(target_os = "macos")]
            app.set_menu(tauri::menu::Menu::default(app.handle())?)?;

            let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
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

            let handle = app.handle().clone();
            let resources = app.path().resource_dir().ok();
            std::thread::spawn(move || match start_engine(resources) {
                Err(message) => show_error(&window, &message),
                Ok((child, port, token)) => {
                    if let Some(state) = handle.try_state::<Engine>() {
                        *state.0.lock().unwrap() = Some(child);
                    }
                    let started = Instant::now();
                    while !engine_ready(port, &token) {
                        if started.elapsed() > Duration::from_secs(40) {
                            show_error(&window, "The engine did not answer within 40 seconds.");
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                    let _ = window.eval(&format!("location.replace('http://127.0.0.1:{port}/?desktop={}')", platform()));
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Kamroll");

    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            kill_engine(handle);
        }
    });
}
