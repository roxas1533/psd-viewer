// リリースビルドの Windows でコンソールウィンドウを出さない
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod blend;
mod composite;
mod dnd;
mod document;
mod fonts;
mod psd;
mod tiles;
mod worker;

use std::path::PathBuf;

use eframe::egui;

fn main() -> eframe::Result {
    use winit::platform::run_on_demand::EventLoopExtRunOnDemand;

    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PSD Viewer")
            .with_app_id("psd-viewer")
            .with_inner_size([1280.0, 800.0])
            .with_drag_and_drop(true),
        glow_options: eframe::egui_glow::GlowConfiguration {
            // Wayland の EGL は垂直同期ありだと、画面更新でコンポジタのフレーム通知を待つ。
            // 非表示のワークスペースでは通知が来ないのでメインスレッドが止まり、
            // Hyprland が「応答しません」を出す。描画間隔は ViewerApp::logic で 60fps に制限する。
            vsync: !cfg!(target_os = "linux"),
            ..Default::default()
        },
        ..Default::default()
    };

    // ドラッグ&ドロップの受け口をウィンドウより先に作るため、イベントループを自前で用意する
    let mut event_loop = winit::event_loop::EventLoop::<eframe::UserEvent>::with_user_event().build()?;
    let (mut drop_listener, drop_inbox) = dnd::start(&event_loop);

    let mut app = eframe::create_native(
        "psd-viewer",
        options,
        Box::new(|cc| {
            fonts::install_japanese_font(&cc.egui_ctx);
            drop_inbox.attach(&cc.egui_ctx);
            Ok(Box::new(app::ViewerApp::new(cc, initial_path, drop_inbox)))
        }),
        &event_loop,
    );
    let result = event_loop.run_app_on_demand(&mut app);

    // 表示の接続を閉じる前に受信スレッドを止める
    drop_listener.stop();
    drop(app);
    result.map_err(Into::into)
}
