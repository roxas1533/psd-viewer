//! ファイルのドラッグ&ドロップ。
//!
//! Wayland では自前で受け取り、それ以外（Windows など）は winit のドロップイベントを使う。

#[cfg(target_os = "linux")]
mod wayland;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, OnceLock};

use eframe::egui;
use raw_window_handle::HasDisplayHandle;

// Wayland 以外ではチャンネルに送る側がいない
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum DropEvent {
    Hover(bool),
    Files(Vec<PathBuf>),
}

/// 受信スレッド。表示の接続より先に止める必要があるので、イベントループと同じ場所で持つ。
pub struct DropListener {
    #[cfg(target_os = "linux")]
    listener: Option<wayland::Listener>,
}

/// アプリ側の受け口
pub struct DropInbox {
    rx: Receiver<DropEvent>,
    hovering: bool,
    ctx: Arc<OnceLock<egui::Context>>,
}

/// ウィンドウを作る前に呼ぶ。
///
/// Hyprland はクライアントが最初に作った `wl_data_device` にしかドラッグを送らない。
/// eframe はウィンドウ作成時にクリップボード用の data device を作るので、それより先に作っておく。
pub fn start(display: &impl HasDisplayHandle) -> (DropListener, DropInbox) {
    let (tx, rx) = channel();
    let ctx = Arc::new(OnceLock::new());

    #[cfg(target_os = "linux")]
    let listener = {
        use raw_window_handle::RawDisplayHandle;
        match display.display_handle().map(|h| h.as_raw()) {
            // SAFETY: wl_display はイベントループが生きている間有効で、main で先に stop している
            Ok(RawDisplayHandle::Wayland(h)) => match unsafe { wayland::Listener::start(h.display, tx, ctx.clone()) } {
                Ok(listener) => Some(listener),
                Err(e) => {
                    eprintln!("ドラッグ&ドロップを初期化できません: {e}");
                    None
                }
            },
            _ => None,
        }
    };
    #[cfg(not(target_os = "linux"))]
    let _ = (display, tx);

    let listener = DropListener {
        #[cfg(target_os = "linux")]
        listener,
    };
    let inbox = DropInbox {
        rx,
        hovering: false,
        ctx,
    };
    (listener, inbox)
}

impl DropListener {
    pub fn stop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(listener) = &mut self.listener {
            listener.stop();
        }
    }
}

impl DropInbox {
    /// 受信時に再描画を要求する先を登録する
    pub fn attach(&self, ctx: &egui::Context) {
        let _ = self.ctx.set(ctx.clone());
    }

    /// ドロップされたファイルがあれば返す。毎フレーム呼ぶ。
    pub fn poll(&mut self, ctx: &egui::Context) -> Option<PathBuf> {
        let mut dropped = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        while let Ok(event) = self.rx.try_recv() {
            match event {
                DropEvent::Hover(h) => self.hovering = h,
                DropEvent::Files(files) => dropped = files.into_iter().next(),
            }
        }
        dropped
    }

    pub fn hovering(&self, ctx: &egui::Context) -> bool {
        self.hovering || ctx.input(|i| !i.raw.hovered_files.is_empty())
    }
}
