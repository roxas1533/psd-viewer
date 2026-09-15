//! レイヤーツリーの合成。
//!
//! 効果やフィルタのような周辺ピクセルを参照する処理がないので、1 行ずつツリー全体を評価する。
//! 作業バッファは「幅 × ネストの深さ」ぶんで済み、行ごとにスレッドへ分配できる。
//! 行バッファは乗算済みアルファの f32 RGBA。レイヤーのピクセルもその行だけを展開する。

use std::sync::Mutex;

use eframe::egui::{Color32, ColorImage};

use crate::blend::BlendMode;
use crate::document::{Document, Node, NodeId, NodeKind, Pixels};

/// 縮小段階 `level`（1/2^level）での出力画素の矩形。座標は負やキャンバス外でもよい（透明になる）。
#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub level: u32,
    pub x: i64,
    pub y: i64,
    pub width: usize,
    pub height: usize,
}

impl Region {
    /// キャンバス上で占める画素数（処理量の目安）
    pub fn canvas_area(&self) -> usize {
        (self.width * self.height) << (2 * self.level)
    }
}

/// 複数の領域をまとめて合成する。全領域の出力行を全コアで分担する。
/// 縮小段階では 2^level × 2^level 画素の平均をとる。
pub fn render_regions(doc: &Document, visible: &[bool], regions: &[Region]) -> Vec<ColorImage> {
    let mut outputs: Vec<Vec<Color32>> = regions
        .iter()
        .map(|r| vec![Color32::TRANSPARENT; r.width * r.height])
        .collect();

    let total_rows: usize = regions.iter().map(|r| r.height).sum();
    if total_rows > 0 && doc.width > 0 && doc.height > 0 {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(total_rows);
        let rows = Mutex::new(outputs.iter_mut().enumerate().flat_map(|(i, pixels)| {
            let width = regions[i].width.max(1);
            pixels.chunks_mut(width).enumerate().map(move |(row, out)| (i, row, out))
        }));
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut ctx = RowContext::new(doc, visible);
                    let mut line = vec![0.0f32; doc.width as usize * 4];
                    let mut sum = Vec::new();
                    loop {
                        let Some((i, row, out)) = rows.lock().unwrap().next() else {
                            break;
                        };
                        render_output_row(&mut ctx, &regions[i], row, out, &mut line, &mut sum);
                    }
                });
            }
        });
    }

    outputs
        .into_iter()
        .zip(regions)
        .map(|(pixels, r)| ColorImage::new([r.width, r.height], pixels))
        .collect()
}

fn render_output_row(ctx: &mut RowContext, region: &Region, row: usize, out: &mut [Color32], line: &mut [f32], sum: &mut Vec<f32>) {
    let doc = ctx.doc;
    let (w, h) = (doc.width as i64, doc.height as i64);
    let scale = 1i64 << region.level;
    // この出力行が覆うキャンバスの範囲
    let cy0 = (region.y + row as i64) * scale;
    let cx0 = region.x * scale;
    let cx1 = (region.x + region.width as i64) * scale;
    let (wx0, wx1) = (cx0.clamp(0, w) as usize, cx1.clamp(0, w) as usize);
    if wx0 >= wx1 {
        return;
    }
    ctx.window = (wx0, wx1);

    if scale == 1 {
        if !(0..h).contains(&cy0) {
            return;
        }
        line[wx0 * 4..wx1 * 4].fill(0.0);
        ctx.render_stack(&doc.root, cy0 as i32, line);
        for cx in wx0..wx1 {
            let c = &line[cx * 4..cx * 4 + 4];
            out[(cx as i64 - cx0) as usize] = to_color(c[0], c[1], c[2], c[3]);
        }
        return;
    }

    sum.clear();
    sum.resize(region.width * 4, 0.0);
    for cy in cy0.max(0)..(cy0 + scale).min(h) {
        line[wx0 * 4..wx1 * 4].fill(0.0);
        ctx.render_stack(&doc.root, cy as i32, line);
        for cx in wx0..wx1 {
            let o = ((cx as i64 - cx0) / scale) as usize * 4;
            for c in 0..4 {
                sum[o + c] += line[cx * 4 + c];
            }
        }
    }
    // キャンバス外の画素は透明として平均に含める
    let inv = 1.0 / (scale * scale) as f32;
    for (px, c) in out.iter_mut().zip(sum.as_chunks::<4>().0) {
        *px = to_color(c[0] * inv, c[1] * inv, c[2] * inv, c[3] * inv);
    }
}

#[inline]
fn to_color(r: f32, g: f32, b: f32, a: f32) -> Color32 {
    Color32::from_rgba_premultiplied(to_u8(r), to_u8(g), to_u8(b), to_u8(a))
}

#[inline]
fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

struct RowContext<'a> {
    doc: &'a Document,
    visible: &'a [bool],
    width: usize,
    /// 今回描く列の範囲。作業はこの範囲に限る。
    window: (usize, usize),
    /// 使い回す一時行バッファ
    pool: Vec<Vec<f32>>,
    /// レイヤーの展開用（アルファ + 色 4 チャンネル）
    channels: [Vec<u8>; 5],
    /// マスクの展開用
    mask_row: Vec<u8>,
    /// マスクを掛け合わせた係数（キャンバス幅）
    coverage: Vec<f32>,
}

impl<'a> RowContext<'a> {
    fn new(doc: &'a Document, visible: &'a [bool]) -> Self {
        let width = doc.width as usize;
        Self {
            doc,
            visible,
            width,
            window: (0, width),
            pool: Vec::new(),
            channels: Default::default(),
            mask_row: Vec::new(),
            coverage: vec![1.0; width],
        }
    }

    /// 列の範囲だけを 0 にした一時行バッファを借りる。範囲外の中身は不定。
    fn take_buf(&mut self) -> Vec<f32> {
        match self.pool.pop() {
            Some(mut buf) => {
                let (x0, x1) = self.window;
                buf[x0 * 4..x1 * 4].fill(0.0);
                buf
            }
            None => vec![0.0; self.width * 4],
        }
    }

    fn give_buf(&mut self, buf: Vec<f32>) {
        self.pool.push(buf);
    }

    /// 兄弟要素（下から上）を `dst` に重ねる。
    fn render_stack(&mut self, children: &[NodeId], y: i32, dst: &mut [f32]) {
        let doc = self.doc;
        let nodes = &doc.nodes;
        let mut i = 0;
        while i < children.len() {
            let base = children[i];
            // 直上に続くクリッピングレイヤーは、このレイヤーを土台にまとめて扱う
            let mut end = i + 1;
            while end < children.len() && nodes[children[end]].clipped {
                end += 1;
            }
            let clips = &children[i + 1..end];
            i = end;

            let node = &nodes[base];
            if !self.visible[base] || !node.bounds.contains_row(y) {
                continue;
            }

            let has_visible_clip = clips.iter().any(|&c| self.visible[c]);
            let pass_through = matches!(node.kind, NodeKind::Group { pass_through: true, .. });

            if pass_through && !has_visible_clip {
                self.render_pass_through(base, y, dst);
                continue;
            }

            let mut buf = self.take_buf();
            self.render_isolated(base, y, &mut buf);
            for &clip in clips {
                if !self.visible[clip] || !nodes[clip].bounds.contains_row(y) {
                    continue;
                }
                let mut clip_buf = self.take_buf();
                self.render_isolated(clip, y, &mut clip_buf);
                let clip_node = &nodes[clip];
                composite_row(&mut buf, &clip_buf, clip_node, self.span(clip_node), true);
                self.give_buf(clip_buf);
            }
            composite_row(dst, &buf, node, self.span(node), false);
            self.give_buf(buf);
        }
    }

    /// ノード単体を透明な `out` に描く（不透明度と描画モードは適用しない）。
    fn render_isolated(&mut self, id: NodeId, y: i32, out: &mut [f32]) {
        let doc = self.doc;
        let node = &doc.nodes[id];
        match &node.kind {
            NodeKind::Layer(pixels) => {
                let mut channels = std::mem::take(&mut self.channels);
                self.render_layer(node, pixels, y, out, &mut channels);
                self.channels = channels;
            }
            NodeKind::Group { children, .. } => {
                self.render_stack(children, y, out);
                let (x0, x1) = self.span(node);
                if self.mask_coverage(node, y, x0, x1) {
                    for cx in x0..x1 {
                        let m = self.coverage[cx];
                        out[cx * 4..cx * 4 + 4].iter_mut().for_each(|v| *v *= m);
                    }
                }
            }
            NodeKind::Unsupported { .. } => {}
        }
    }

    fn render_layer(&mut self, node: &Node, pixels: &Pixels, y: i32, out: &mut [f32], ch: &mut [Vec<u8>; 5]) {
        let doc = self.doc;
        let data: &[u8] = &doc.data;
        let (x0, x1) = self.span(node);
        if x0 >= x1 {
            return;
        }
        let layer_width = (pixels.rect.x1 - pixels.rect.x0) as usize;
        let row = (y - pixels.rect.y0) as usize;
        let offset = pixels.rect.x0;
        // キャンバス座標 → レイヤー内の列
        let (lx0, lx1) = ((x0 as i32 - offset) as usize, (x1 as i32 - offset) as usize);
        for buf in ch.iter_mut() {
            if buf.len() < layer_width {
                buf.resize(layer_width, 0);
            }
        }

        // 透明な行は色を展開しない
        match &pixels.alpha {
            Some(alpha) => {
                alpha.read_row(data, row, &mut ch[0]);
                if ch[0][lx0..lx1].iter().all(|&a| a == 0) {
                    return;
                }
            }
            None => ch[0][lx0..lx1].fill(255),
        }
        let has_mask = self.mask_coverage(node, y, x0, x1);

        let color_count = match doc.color_mode {
            1 => 1,
            4 => 4,
            _ => 3,
        };
        // CMYK は反転して格納されているので、欠けたチャンネルは 255（インクなし）
        let missing = if doc.color_mode == 4 { 255 } else { 0 };
        for c in 0..color_count {
            match &pixels.color[c] {
                Some(channel) => channel.read_row(data, row, &mut ch[c + 1]),
                None => ch[c + 1][lx0..lx1].fill(missing),
            }
        }

        let fill = node.fill_opacity;
        for cx in x0..x1 {
            let lx = (cx as i32 - offset) as usize;
            let mut a = ch[0][lx] as f32 / 255.0 * fill;
            if has_mask {
                a *= self.coverage[cx];
            }
            if a <= 0.0 {
                continue;
            }
            let rgb = match color_count {
                1 => [ch[1][lx]; 3].map(|v| v as f32),
                4 => {
                    let k = ch[4][lx] as f32 / 255.0;
                    [ch[1][lx], ch[2][lx], ch[3][lx]].map(|v| v as f32 * k)
                }
                _ => [ch[1][lx], ch[2][lx], ch[3][lx]].map(|v| v as f32),
            };
            let k = a / 255.0;
            let o = &mut out[cx * 4..cx * 4 + 4];
            o[0] = rgb[0] * k;
            o[1] = rgb[1] * k;
            o[2] = rgb[2] * k;
            o[3] = a;
        }
    }

    /// ノードのマスクを `coverage[x0..x1]` に書き出す。マスクがなければ false。
    fn mask_coverage(&mut self, node: &Node, y: i32, x0: usize, x1: usize) -> bool {
        if node.masks.is_empty() {
            return false;
        }
        let data: &[u8] = &self.doc.data;
        self.coverage[x0..x1].fill(1.0);
        for mask in &node.masks {
            let value = |v: u8| {
                let v = v as f32 / 255.0;
                if mask.invert { 1.0 - v } else { v }
            };
            let rect = mask.rect;
            if !rect.contains_row(y) || mask.channel.width == 0 {
                let v = value(mask.default);
                self.coverage[x0..x1].iter_mut().for_each(|c| *c *= v);
                continue;
            }
            let w = mask.channel.width;
            if self.mask_row.len() < w {
                self.mask_row.resize(w, 0);
            }
            mask.channel.read_row(data, (y - rect.y0) as usize, &mut self.mask_row);
            for cx in x0..x1 {
                let mx = cx as i32 - rect.x0;
                let raw = if mx >= 0 && (mx as usize) < w {
                    self.mask_row[mx as usize]
                } else {
                    mask.default
                };
                self.coverage[cx] *= value(raw);
            }
        }
        true
    }

    /// 通過グループ: 子を背景に直接重ね、不透明度とマスクで元の背景と混ぜる。
    fn render_pass_through(&mut self, id: NodeId, y: i32, dst: &mut [f32]) {
        let doc = self.doc;
        let node = &doc.nodes[id];
        let NodeKind::Group { children, .. } = &node.kind else {
            return;
        };
        if node.opacity >= 1.0 && node.masks.is_empty() {
            self.render_stack(children, y, dst);
            return;
        }
        let mut buf = self.take_buf();
        let (w0, w1) = self.window;
        buf[w0 * 4..w1 * 4].copy_from_slice(&dst[w0 * 4..w1 * 4]);
        self.render_stack(children, y, &mut buf);
        let (x0, x1) = self.span(node);
        let has_mask = self.mask_coverage(node, y, x0, x1);
        for cx in x0..x1 {
            let mut t = node.opacity;
            if has_mask {
                t *= self.coverage[cx];
            }
            for c in cx * 4..cx * 4 + 4 {
                dst[c] += (buf[c] - dst[c]) * t;
            }
        }
        self.give_buf(buf);
    }

    /// ノードの矩形と描画範囲の重なり
    fn span(&self, node: &Node) -> (usize, usize) {
        let b = node.bounds;
        let (w0, w1) = self.window;
        let x0 = (b.x0.max(0) as usize).max(w0);
        let x1 = (b.x1.max(0) as usize).min(w1);
        (x0, x1.max(x0))
    }
}

/// 乗算済みの `src` を `dst` に重ねる。`atop` はクリッピング（結果のアルファは背景のまま）。
fn composite_row(dst: &mut [f32], src: &[f32], node: &Node, (x0, x1): (usize, usize), atop: bool) {
    let opacity = node.opacity;
    let blend = node.blend;
    for cx in x0..x1 {
        let i = cx * 4;
        let sa = src[i + 3] * opacity;
        if sa <= 0.0 {
            continue;
        }
        let d = &mut dst[i..i + 4];
        let da = d[3];
        let fa = if atop { da } else { 1.0 };
        let inv = 1.0 - sa;

        if blend == BlendMode::Normal {
            for c in 0..3 {
                d[c] = src[i + c] * opacity * fa + d[c] * inv;
            }
        } else {
            let s = [0, 1, 2].map(|c| src[i + c] / src[i + 3]);
            let b = if da > 0.0 { [0, 1, 2].map(|c| d[c] / da) } else { [0.0; 3] };
            let mixed = blend.apply(b, s);
            for c in 0..3 {
                let m = (1.0 - da) * s[c] + da * mixed[c];
                d[c] = sa * fa * m + d[c] * inv;
            }
        }
        d[3] = sa * fa + da * inv;
    }
}
