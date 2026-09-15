//! 表示用のタイル。
//!
//! 画像を縮小段階（1/2^level）ごとに `TILE_SIZE` 四方へ分け、合成済みのタイルを
//! テクスチャとしてキャッシュする。画素は GPU 側にだけ置き、上限を超えたら古いものから捨てる。

use std::collections::HashMap;

use eframe::egui::{
    self, Color32, ColorImage, Painter, Pos2, Rect, TextureFilter, TextureHandle, TextureOptions, TextureWrapMode, pos2,
    vec2,
};

use crate::composite::Region;
use crate::document::Document;

pub const TILE_SIZE: usize = 512;

/// 隣のタイルとの境目で線形補間が途切れないよう、周囲に余分に描く画素数
const PAD: usize = 1;

const OPTIONS: TextureOptions = TextureOptions {
    magnification: TextureFilter::Linear,
    minification: TextureFilter::Linear,
    wrap_mode: TextureWrapMode::ClampToEdge,
    mipmap_mode: None,
};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TileKey {
    pub level: u32,
    pub x: u32,
    pub y: u32,
}

impl TileKey {
    /// パディングを除いた出力画素数
    fn content_size(&self, doc: &Document) -> (usize, usize) {
        let (lw, lh) = level_size(doc, self.level);
        let x0 = self.x as usize * TILE_SIZE;
        let y0 = self.y as usize * TILE_SIZE;
        (TILE_SIZE.min(lw.saturating_sub(x0)), TILE_SIZE.min(lh.saturating_sub(y0)))
    }

    /// 合成する領域（パディング込み）
    pub fn region(&self, doc: &Document) -> Region {
        let (w, h) = self.content_size(doc);
        Region {
            level: self.level,
            x: (self.x as usize * TILE_SIZE) as i64 - PAD as i64,
            y: (self.y as usize * TILE_SIZE) as i64 - PAD as i64,
            width: w + PAD * 2,
            height: h + PAD * 2,
        }
    }

    /// キャンバス座標での矩形
    fn canvas_rect(&self, doc: &Document) -> Rect {
        let scale = (1u64 << self.level) as f32;
        let (w, h) = self.content_size(doc);
        let min = pos2(self.x as f32 * TILE_SIZE as f32 * scale, self.y as f32 * TILE_SIZE as f32 * scale);
        Rect::from_min_size(min, vec2(w as f32 * scale, h as f32 * scale))
    }
}

/// 段階 `level` での画像の大きさ
fn level_size(doc: &Document, level: u32) -> (usize, usize) {
    let scale = 1usize << level;
    ((doc.width as usize).div_ceil(scale), (doc.height as usize).div_ceil(scale))
}

/// 画像全体が 1 枚のタイルに収まる段階
pub fn max_level(doc: &Document) -> u32 {
    let mut level = 0;
    loop {
        let (w, h) = level_size(doc, level);
        if w <= TILE_SIZE && h <= TILE_SIZE {
            return level;
        }
        level += 1;
    }
}

/// 表示倍率（物理画素 / キャンバス画素）に合う段階。
/// 少しだけ細かい段階を優先し、GPU での縮小は最大約 1.7 倍、拡大は約 1.2 倍に収める。
pub fn level_for_scale(scale: f32, max: u32) -> u32 {
    if scale >= 1.0 {
        return 0;
    }
    ((-scale.log2() + 0.25).floor().max(0.0) as u32).min(max)
}

/// 段階 `level` で、キャンバス上の矩形 `view` に重なるタイル（`view` の中心に近い順）
pub fn tiles_in(doc: &Document, level: u32, view: Rect) -> Vec<TileKey> {
    let (lw, lh) = level_size(doc, level);
    let (count_x, count_y) = (lw.div_ceil(TILE_SIZE) as i64, lh.div_ceil(TILE_SIZE) as i64);
    let span = (TILE_SIZE as u64 * (1u64 << level)) as f32;
    let x0 = ((view.min.x / span).floor() as i64).clamp(0, count_x);
    let x1 = ((view.max.x / span).ceil() as i64).clamp(0, count_x);
    let y0 = ((view.min.y / span).floor() as i64).clamp(0, count_y);
    let y1 = ((view.max.y / span).ceil() as i64).clamp(0, count_y);

    let mut keys: Vec<TileKey> = (y0..y1)
        .flat_map(|y| (x0..x1).map(move |x| TileKey { level, x: x as u32, y: y as u32 }))
        .collect();
    let center = view.center();
    keys.sort_by(|a, b| {
        let da = a.canvas_rect(doc).center().distance_sq(center);
        let db = b.canvas_rect(doc).center().distance_sq(center);
        da.total_cmp(&db)
    });
    keys
}

struct Tile {
    texture: TextureHandle,
    /// 合成したときの表示状態の世代
    generation: u64,
    last_used: u64,
    bytes: usize,
}

#[derive(Default)]
pub struct TileCache {
    tiles: HashMap<TileKey, Tile>,
    bytes: usize,
}

impl TileCache {
    pub fn clear(&mut self) {
        self.tiles.clear();
        self.bytes = 0;
    }

    pub fn insert(&mut self, ctx: &egui::Context, key: TileKey, generation: u64, image: ColorImage, frame: u64) {
        let bytes = image.pixels.len() * 4;
        let name = format!("tile-{}-{}-{}", key.level, key.x, key.y);
        let tile = Tile {
            texture: ctx.load_texture(name, image, OPTIONS),
            generation,
            last_used: frame,
            bytes,
        };
        self.bytes += bytes;
        if let Some(old) = self.tiles.insert(key, tile) {
            self.bytes -= old.bytes;
        }
    }

    /// 世代 `generation` のタイルがあるか
    pub fn is_ready(&self, key: &TileKey, generation: u64) -> bool {
        self.tiles.get(key).is_some_and(|t| t.generation == generation)
    }

    /// タイルがあれば描く。古い世代のものでも、差し替わるまでの仮表示として描く。
    pub fn paint(&mut self, painter: &Painter, doc: &Document, key: &TileKey, origin: Pos2, zoom: f32, frame: u64) {
        let Some(tile) = self.tiles.get_mut(key) else { return };
        tile.last_used = frame;
        let [tw, th] = tile.texture.size();
        let uv = Rect::from_min_max(
            pos2(PAD as f32 / tw as f32, PAD as f32 / th as f32),
            pos2((tw - PAD) as f32 / tw as f32, (th - PAD) as f32 / th as f32),
        );
        let canvas = key.canvas_rect(doc);
        let screen = Rect::from_min_size(origin + canvas.min.to_vec2() * zoom, canvas.size() * zoom);
        painter.image(tile.texture.id(), screen, uv, Color32::WHITE);
    }

    /// 合計が `budget` バイトを超えていたら、このフレームで使っていないものを古い順に捨てる。
    /// 古い世代のタイルを優先して捨てる。
    pub fn evict(&mut self, budget: usize, frame: u64, generation: u64) {
        if self.bytes <= budget {
            return;
        }
        let mut candidates: Vec<(bool, u64, TileKey)> = self
            .tiles
            .iter()
            .filter(|(_, t)| t.last_used < frame)
            .map(|(k, t)| (t.generation == generation, t.last_used, *k))
            .collect();
        candidates.sort_unstable_by_key(|&(current, used, _)| (current, used));
        for (_, _, key) in candidates {
            if self.bytes <= budget {
                break;
            }
            if let Some(tile) = self.tiles.remove(&key) {
                self.bytes -= tile.bytes;
            }
        }
    }
}
