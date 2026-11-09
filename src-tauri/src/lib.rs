mod audio;
mod config;
mod dest;
mod device;
mod encode;
mod engine;
mod jobs;
mod server;
mod telegram;
mod wav;
#[cfg(test)]
mod testutil;

use engine::{Engine, WindowCmd};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    log::info!("PracticeSpaceRecorder {} starting", env!("CARGO_PKG_VERSION"));

    let engine = Engine::start_with(&config::config_dir().join("config.json"), config::data_dir(), device::CpalBackend, engine::keep_awake);
    let settings = engine.settings();
    let port = tauri::async_runtime::block_on(server::serve(engine.clone(), settings.port)).expect("cannot start UI server");
    engine.set_server_info(port);

    let eng_setup = engine.clone();
    let eng_close = engine.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
            let url = format!("http://127.0.0.1:{port}/").parse().expect("url");
            let win = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(url))
                .title("PracticeSpaceRecorder")
                .inner_size(1440.0, 900.0)
                .min_inner_size(960.0, 620.0)
                .background_color(tauri::window::Color(18, 17, 22, 255))
                .fullscreen(settings.fullscreen)
                .focused(true)
                .build()?;
            let _ = win.set_focus();
            let handle = app.handle().clone();
            let eng_pick = std::sync::Arc::downgrade(&eng_setup);
            eng_setup.set_window_handler(Box::new(move |cmd| match cmd {
                WindowCmd::Fullscreen(on) => {
                    if let Some(w) = handle.get_webview_window("main") {
                        let _ = w.set_fullscreen(on);
                    }
                }
                WindowCmd::PickFolder { req, start } => {
                    let eng = eng_pick.clone();
                    let mut dlg = handle.dialog().file().set_title("Choose where your takes go");
                    if !start.is_empty() && std::path::Path::new(&start).is_dir() {
                        dlg = dlg.set_directory(&start);
                    }
                    dlg.pick_folder(move |p| {
                        if let Some(e) = eng.upgrade() {
                            e.picked(&req, p.and_then(|p| p.into_path().ok()).map(|p| p.to_string_lossy().into_owned()));
                        }
                    });
                }
                WindowCmd::Quit => handle.exit(0),
            }));
            Ok(())
        })
        .on_window_event(move |_w, ev| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
                if eng_close.is_recording() {
                    // closing mid-take would throw away the band's best song ever. Probably.
                    api.prevent_close();
                    eng_close.handle(r#"{"cmd":"stop"}"#, true);
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running PracticeSpaceRecorder");
}
