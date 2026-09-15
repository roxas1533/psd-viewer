use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use eframe::egui::{
    self, Color32, ColorImage, Key, KeyboardShortcut, Modifiers, Pos2, Rect, RichText, Sense, TextureHandle,
    TextureOptions, TextureWrapMode, Vec2, pos2, vec2,
};

use crate::dnd::DropInbox;
use crate::document::{Document, NodeId, NodeKind};
use crate::tiles::{self, TileCache, TileKey};
use crate::worker::{Request, Response, Worker};

/// タイルキャッシュの下限。実際の上限は表示領域 4 枚分とこの値の大きいほう。
const MIN_CACHE_BYTES: usize = 128 << 20;

const OPEN_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
const QUIT_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Q);

const MIN_ZOOM: f32 = 0.01;
const MAX_ZOOM: f32 = 64.0;

pub struct ViewerApp {
    worker: Worker,
    checker: TextureHandle,
    file_drop: DropInbox,
    /// 開いているファイル選択ダイアログの結果
    dialog: Option<Receiver<Option<PathBuf>>>,

    path: Option<PathBuf>,
    doc: Option<Arc<Document>>,
    /// 開いたファイルごとに変わる。UI の折りたたみ状態の ID に使う。
    doc_serial: u64,
    visible: Vec<bool>,
    /// 合成スレッドへ渡す `visible` の写し（世代が変わるたびに作る）
    visible_shared: Arc<Vec<bool>>,

    tiles: TileCache,
    /// 表示状態の世代。ファイルを開いたときとレイヤーを切り替えたときに進む。
    generation: u64,
    /// 要求済みでまだ届いていないタイル
    pending: HashSet<TileKey>,
    /// 受け取った `Response::Tile` の数
    received: u64,
    frame: u64,
    loading: bool,
    last_compose: Option<Duration>,
    error: Option<String>,

    zoom: f32,
    pan: Vec2,
    fit_pending: bool,
}

impl ViewerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_path: Option<PathBuf>, file_drop: DropInbox) -> Self {
        let ctx = &cc.egui_ctx;
        let (light, dark) = (Color32::from_gray(200), Color32::from_gray(150));
        let checker = ctx.load_texture(
            "checker",
            ColorImage::new([2, 2], vec![light, dark, dark, light]),
            TextureOptions {
                wrap_mode: TextureWrapMode::Repeat,
                ..TextureOptions::NEAREST
            },
        );

        let mut app = Self {
            worker: Worker::spawn(ctx.clone()),
            checker,
            file_drop,
            dialog: None,
            path: None,
            doc: None,
            doc_serial: 0,
            visible: Vec::new(),
            visible_shared: Arc::default(),
            tiles: TileCache::default(),
            generation: 0,
            pending: HashSet::new(),
            received: 0,
            frame: 0,
            loading: false,
            last_compose: None,
            error: None,
            zoom: 1.0,
            pan: Vec2::ZERO,
            fit_pending: false,
        };
        if let Some(path) = initial_path {
            app.open(path);
        }
        app
    }

    fn open(&mut self, path: PathBuf) {
        self.loading = true;
        self.error = None;
        self.worker.send(Request::Open(path));
    }

    /// ダイアログは UI を止めないよう別スレッドで開く。
    fn open_dialog(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let directory = self.path.as_ref().and_then(|p| p.parent()).map(|p| p.to_path_buf());
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new()
                .set_title("PSD を開く")
                .add_filter("Photoshop", &["psd", "psb"]);
            if let Some(dir) = directory {
                dialog = dialog.set_directory(dir);
            }
            let _ = tx.send(dialog.pick_file());
            ctx.request_repaint();
        });
        self.dialog = Some(rx);
    }

    fn poll_dialog(&mut self) {
        let Some(rx) = &self.dialog else { return };
        match rx.try_recv() {
            Ok(picked) => {
                self.dialog = None;
                if let Some(path) = picked {
                    self.open(path);
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.dialog = None,
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("ファイル", |ui| {
                let open = egui::Button::new("開く…").shortcut_text(ui.ctx().format_shortcut(&OPEN_SHORTCUT));
                if ui.add_enabled(self.dialog.is_none(), open).clicked() {
                    self.open_dialog(ui.ctx());
                }
                ui.separator();
                let quit = egui::Button::new("終了").shortcut_text(ui.ctx().format_shortcut(&QUIT_SHORTCUT));
                if ui.add(quit).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }

    /// 表示状態が変わった。キャッシュ済みのタイルは差し替わるまで仮表示に使う。
    fn next_generation(&mut self) {
        self.generation += 1;
        self.visible_shared = Arc::new(self.visible.clone());
        self.pending.clear();
        self.last_compose = None;
    }

    /// 足りないタイルのうち、まだ要求していないものがあれば一覧ごと要求し直す。
    fn request_tiles(&mut self, doc: &Arc<Document>, missing: Vec<TileKey>) {
        if missing.iter().all(|k| self.pending.contains(k)) {
            return;
        }
        self.pending = missing.iter().copied().collect();
        self.worker.send(Request::Tiles {
            doc: doc.clone(),
            visible: self.visible_shared.clone(),
            generation: self.generation,
            tiles: missing,
            received: self.received,
        });
    }

    fn handle_responses(&mut self, ctx: &egui::Context) {
        while let Some(res) = self.worker.try_recv() {
            match res {
                Response::Opened { path, result } => {
                    self.loading = false;
                    match result {
                        Ok(doc) => {
                            let title = path.file_name().map_or_else(
                                || "PSD Viewer".to_owned(),
                                |n| format!("{} - PSD Viewer", n.to_string_lossy()),
                            );
                            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
                            self.visible = doc.initial_visibility();
                            self.doc = Some(doc);
                            self.doc_serial += 1;
                            self.path = Some(path);
                            self.tiles.clear();
                            self.fit_pending = true;
                            self.next_generation();
                        }
                        Err(e) => self.error = Some(format!("{}: {e}", path.display())),
                    }
                }
                Response::Tile { generation, key, image } => {
                    self.received += 1;
                    // 古い世代の結果は捨てる
                    if generation == self.generation {
                        self.pending.remove(&key);
                        self.tiles.insert(ctx, key, generation, image, self.frame);
                    }
                }
                Response::Idle { generation, elapsed } => {
                    if generation == self.generation {
                        self.last_compose = Some(elapsed);
                    }
                }
            }
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("全体表示").on_hover_text("F / ダブルクリック").clicked() {
                self.fit_pending = true;
            }
            if ui.button("100%").on_hover_text("1").clicked() {
                self.zoom = 1.0;
                self.pan = Vec2::ZERO;
            }
            ui.label(format!("{:.0}%", self.zoom * 100.0));
            ui.separator();

            if let Some(doc) = &self.doc {
                ui.label(format!("{} × {} px", doc.width, doc.height));
            }
            if self.loading {
                ui.spinner();
                ui.label("読み込み中…");
            } else if !self.pending.is_empty() {
                ui.spinner();
                ui.label("合成中…");
            } else if let Some(t) = self.last_compose {
                ui.label(RichText::new(format!("合成 {} ms", t.as_millis())).weak());
            }
            if let Some(err) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, err);
            }
        });
    }

    fn layer_panel(&mut self, ui: &mut egui::Ui) {
        let Some(doc) = self.doc.clone() else {
            ui.weak("PSD が開かれていません");
            return;
        };

        let mut changed = false;
        ui.horizontal(|ui| {
            if ui.button("初期状態に戻す").clicked() {
                self.visible = doc.initial_visibility();
                changed = true;
            }
        });
        for warning in &doc.warnings {
            ui.colored_label(ui.visuals().warn_fg_color, warning);
        }
        ui.separator();

        egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            changed |= layer_tree(ui, &doc, &mut self.visible, &doc.root, true, self.doc_serial);
        });

        if changed {
            self.next_generation();
        }
    }

    fn canvas(&mut self, ui: &mut egui::Ui) {
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(40));

        let Some(doc) = self.doc.clone() else {
            let text = if self.loading { "読み込み中…" } else { "PSDをドラッグ&ドロップ" };
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(20.0),
                Color32::from_gray(160),
            );
            self.paint_drop_overlay(ui, rect);
            return;
        };
        let image_size = vec2(doc.width as f32, doc.height as f32);

        let (fit_key, one_key) = ui.input(|i| (i.key_pressed(Key::F), i.key_pressed(Key::Num1)));
        if self.fit_pending || fit_key || response.double_clicked() {
            self.fit_pending = false;
            let scale = (rect.size() / image_size).min_elem() * 0.95;
            self.zoom = scale.clamp(MIN_ZOOM, MAX_ZOOM);
            self.pan = Vec2::ZERO;
        }
        if one_key {
            self.zoom = 1.0;
            self.pan = Vec2::ZERO;
        }

        if response.dragged() {
            self.pan += response.drag_delta();
        }
        if let Some(pointer) = response.hover_pos() {
            let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            let factor = pinch * (scroll * 0.002).exp();
            if factor != 1.0 {
                self.zoom_about(rect, pointer, factor);
            }
        }

        let image_rect = Rect::from_center_size(rect.center() + self.pan, image_size * self.zoom);
        let tiles = image_rect.size() / 16.0;
        painter.image(
            self.checker.id(),
            image_rect,
            Rect::from_min_max(Pos2::ZERO, pos2(tiles.x, tiles.y)),
            Color32::WHITE,
        );
        self.paint_tiles(ui, &painter, &doc, rect, image_rect);
        self.paint_drop_overlay(ui, rect);
    }

    /// 表示倍率に合う段階のタイルを描き、足りないものを要求する。
    fn paint_tiles(&mut self, ui: &egui::Ui, painter: &egui::Painter, doc: &Arc<Document>, rect: Rect, image_rect: Rect) {
        let ppp = ui.ctx().pixels_per_point();
        let max_level = tiles::max_level(doc);
        let level = tiles::level_for_scale(self.zoom * ppp, max_level);

        let canvas = Rect::from_min_size(Pos2::ZERO, vec2(doc.width as f32, doc.height as f32));
        let to_canvas = |p: Pos2| ((p - image_rect.min) / self.zoom).to_pos2();
        let view = Rect::from_min_max(to_canvas(rect.min), to_canvas(rect.max)).intersect(canvas);

        let wanted = if view.is_positive() { tiles::tiles_in(doc, level, view) } else { Vec::new() };
        // 最も粗い段階は画像全体の仮表示に使うので、常に用意しておく
        let coarse = tiles::tiles_in(doc, max_level, canvas);

        if level != max_level {
            for key in &coarse {
                self.tiles.paint(painter, doc, key, image_rect.min, self.zoom, self.frame);
            }
        }
        for key in &wanted {
            self.tiles.paint(painter, doc, key, image_rect.min, self.zoom, self.frame);
        }

        let mut missing: Vec<TileKey> = Vec::new();
        for key in wanted.iter().chain(&coarse) {
            if !self.tiles.is_ready(key, self.generation) && !missing.contains(key) {
                missing.push(*key);
            }
        }
        self.request_tiles(doc, missing);

        let screen_bytes = (rect.width() * ppp) as usize * (rect.height() * ppp) as usize * 4;
        self.tiles.evict(MIN_CACHE_BYTES.max(screen_bytes * 4), self.frame, self.generation);
    }

    fn paint_drop_overlay(&self, ui: &egui::Ui, rect: Rect) {
        if !self.file_drop.hovering(ui.ctx()) {
            return;
        }
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_black_alpha(160));
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "ドロップして開く",
            egui::FontId::proportional(28.0),
            Color32::WHITE,
        );
    }

    /// `pointer` の下の画像上の点が動かないように拡大縮小する。
    fn zoom_about(&mut self, rect: Rect, pointer: Pos2, factor: f32) {
        let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let image_center = rect.center() + self.pan;
        let ratio = new_zoom / self.zoom;
        self.pan = (pointer - rect.center()) - (pointer - image_center) * ratio;
        self.zoom = new_zoom;
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.frame += 1;
        self.handle_responses(&ctx);

        self.poll_dialog();
        if let Some(path) = self.file_drop.poll(&ctx) {
            self.open(path);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&OPEN_SHORTCUT)) {
            self.open_dialog(&ctx);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&QUIT_SHORTCUT)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::left("layers")
            .resizable(true)
            .default_size(280.0)
            .show(ui, |ui| self.layer_panel(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.canvas(ui));
    }
}

/// 上から順に表示する。戻り値は表示状態が変わったかどうか。
fn layer_tree(
    ui: &mut egui::Ui,
    doc: &Document,
    visible: &mut [bool],
    ids: &[NodeId],
    parent_visible: bool,
    serial: u64,
) -> bool {
    let mut changed = false;
    for &id in ids.iter().rev() {
        let node = &doc.nodes[id];
        let effective = parent_visible && visible[id];

        let mut label = String::new();
        if node.clipped {
            label.push_str("↳ ");
        }
        label.push_str(&node.name);
        let mut text = RichText::new(label);
        if !effective {
            text = text.weak();
        }

        let mut tags = Vec::new();
        if node.opacity < 1.0 {
            tags.push(format!("{:.0}%", node.opacity * 100.0));
        }
        if node.has_effects {
            tags.push("fx 未対応".to_owned());
        }
        if let NodeKind::Unsupported { reason } = &node.kind {
            tags.push(reason.clone());
        }

        let row = |ui: &mut egui::Ui, visible: &mut [bool]| {
            let changed = ui.checkbox(&mut visible[id], text).changed();
            if !tags.is_empty() {
                ui.label(RichText::new(tags.join(" / ")).small().weak());
            }
            changed
        };

        match &node.kind {
            NodeKind::Group { children, expanded, .. } => {
                let state_id = ui.make_persistent_id(("group", serial, id));
                let state =
                    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), state_id, *expanded);
                let (_, header, body) = state
                    .show_header(ui, |ui| row(ui, visible))
                    .body(|ui| layer_tree(ui, doc, visible, children, effective, serial));
                changed |= header.inner || body.is_some_and(|b| b.inner);
            }
            _ => {
                changed |= ui
                    .horizontal(|ui| {
                        // グループの展開ボタンと同じ幅を空け、兄弟のチェックボックスと列をそろえる
                        ui.add_space(ui.spacing().indent);
                        row(ui, visible)
                    })
                    .inner;
            }
        }
    }
    changed
}
