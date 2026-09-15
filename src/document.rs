//! PSD のフラットなレコード列を、合成と UI で扱いやすいツリーに変換する。

use std::fs::File;
use std::ops::Deref;
use std::path::Path;

use memmap2::Mmap;

use crate::blend::BlendMode;
use crate::psd::{self, Channel, Compression, LayerRecord, MaskInfo};

pub type NodeId = usize;

/// キャンバス座標の矩形（右端・下端は含まない）。
#[derive(Clone, Copy, Debug, Default)]
pub struct Bounds {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Bounds {
    pub fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    pub fn contains_row(&self, y: i32) -> bool {
        self.y0 <= y && y < self.y1
    }

    fn union(self, other: Bounds) -> Bounds {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Bounds {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    fn intersect(self, other: Bounds) -> Bounds {
        Bounds {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }
}

impl From<psd::Rect> for Bounds {
    fn from(r: psd::Rect) -> Bounds {
        Bounds {
            x0: r.left,
            y0: r.top,
            x1: r.right,
            y1: r.bottom,
        }
    }
}

pub struct Mask {
    /// マスクデータの矩形（キャンバスで切り詰めない）
    pub rect: Bounds,
    /// 矩形の外側の値
    pub default: u8,
    pub invert: bool,
    pub channel: Channel,
}

pub struct Pixels {
    pub rect: Bounds,
    /// RGB / CMYK / グレースケールの色チャンネル（モードに応じて先頭から使う）
    pub color: [Option<Channel>; 4],
    pub alpha: Option<Channel>,
}

pub enum NodeKind {
    Layer(Box<Pixels>),
    Group {
        /// 下から上の順
        children: Vec<NodeId>,
        pass_through: bool,
        expanded: bool,
    },
    /// 調整レイヤーなど、描画に対応していないもの
    Unsupported { reason: String },
}

pub struct Node {
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    pub fill_opacity: f32,
    pub blend: BlendMode,
    pub clipped: bool,
    pub has_effects: bool,
    pub masks: Vec<Mask>,
    pub bounds: Bounds,
    pub kind: NodeKind,
}

/// PSD ファイルの中身。通常はメモリマップで、OS が必要に応じてページを読み書きする。
pub enum Source {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

impl Deref for Source {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Source::Mapped(map) => map,
            Source::Owned(bytes) => bytes,
        }
    }
}

pub struct Document {
    pub width: u32,
    pub height: u32,
    pub color_mode: u16,
    /// ファイルの中身。チャンネルはここを参照して行単位で展開する。
    pub data: Source,
    pub nodes: Vec<Node>,
    /// 最上位の要素（下から上の順）
    pub root: Vec<NodeId>,
    pub warnings: Vec<String>,
}

impl Document {
    pub fn open(path: &Path) -> Result<Document, String> {
        let file = File::open(path).map_err(|e| format!("開けません: {e}"))?;
        let len = file.metadata().map_err(|e| format!("開けません: {e}"))?.len();
        let data = if len == 0 {
            Source::Owned(Vec::new())
        } else {
            // SAFETY: 開いている間に他のプロセスがファイルを切り詰めると SIGBUS になりうる。
            // 読み取り専用のビューアなので、メモリ使用量を抑える利点を優先する。
            let map = unsafe { Mmap::map(&file) }.map_err(|e| format!("読み込みに失敗しました: {e}"))?;
            Source::Mapped(map)
        };
        Self::from_source(data)
    }

    pub fn from_source(data: Source) -> Result<Document, String> {
        let psd = psd::parse(&data)?;
        let canvas = Bounds {
            x0: 0,
            y0: 0,
            x1: psd.width as i32,
            y1: psd.height as i32,
        };

        let mut warnings = psd.warnings;
        let mut nodes: Vec<Node> = Vec::with_capacity(psd.layers.len());
        // 構築中のグループの子リスト。先頭がルート。
        let mut stack: Vec<Vec<NodeId>> = vec![Vec::new()];

        // レコードは下から上の順。グループは「終端」→子→「フォルダ」の順に現れる。
        for rec in psd.layers {
            if rec.divider == 3 {
                stack.push(Vec::new());
                continue;
            }

            let group = if matches!(rec.divider, 1 | 2) {
                let children = if stack.len() > 1 {
                    stack.pop().unwrap()
                } else {
                    warnings.push(format!("グループ「{}」の終端が見つかりません", rec.name));
                    Vec::new()
                };
                let bounds = children
                    .iter()
                    .fold(Bounds::default(), |acc, &c| acc.union(nodes[c].bounds));
                Some((children, bounds))
            } else {
                None
            };
            nodes.push(make_node(rec, group, canvas));
            stack.last_mut().unwrap().push(nodes.len() - 1);
        }

        if stack.len() > 1 {
            warnings.push("閉じられていないグループがあります".into());
        }
        let mut root: Vec<NodeId> = stack.into_iter().flatten().collect();

        if nodes.is_empty() && !psd.merged.is_empty() {
            warnings.push("レイヤー情報がないため統合画像を表示しています".into());
            let mut color = [None, None, None, None];
            for (slot, channel) in color.iter_mut().zip(psd.merged) {
                *slot = Some(channel);
            }
            nodes.push(Node {
                name: "統合画像".into(),
                visible: true,
                opacity: 1.0,
                fill_opacity: 1.0,
                blend: BlendMode::Normal,
                clipped: false,
                has_effects: false,
                masks: Vec::new(),
                bounds: canvas,
                kind: NodeKind::Layer(Box::new(Pixels {
                    rect: canvas,
                    color,
                    alpha: None,
                })),
            });
            root.push(0);
        }

        Ok(Document {
            width: psd.width,
            height: psd.height,
            color_mode: psd.color_mode,
            data,
            nodes,
            root,
            warnings,
        })
    }

    pub fn initial_visibility(&self) -> Vec<bool> {
        self.nodes.iter().map(|n| n.visible).collect()
    }
}

fn make_node(mut rec: LayerRecord, group: Option<(Vec<NodeId>, Bounds)>, canvas: Bounds) -> Node {
    let mut masks = Vec::new();
    let mut color = [None, None, None, None];
    let mut alpha = None;
    let mut unsupported_compression = None;

    for (id, channel) in rec.channels.drain(..) {
        if let Compression::Unsupported(c) = channel.compression {
            unsupported_compression = Some(c);
        }
        match id {
            0..=3 => color[id as usize] = Some(channel),
            -1 => alpha = Some(channel),
            -2 => push_mask(&mut masks, rec.user_mask, channel),
            -3 => push_mask(&mut masks, rec.real_mask, channel),
            _ => {}
        }
    }

    let (kind, bounds) = if let Some((children, bounds)) = group {
        let kind = NodeKind::Group {
            children,
            pass_through: &rec.blend == b"pass",
            expanded: rec.divider == 1,
        };
        (kind, bounds)
    } else if rec.damaged {
        let reason = "データが壊れています".to_owned();
        (NodeKind::Unsupported { reason }, Bounds::default())
    } else if let Some(name) = rec.adjustment {
        let reason = format!("{name}レイヤーは未対応");
        (NodeKind::Unsupported { reason }, Bounds::default())
    } else if let Some(c) = unsupported_compression {
        let reason = format!("圧縮方式 {c} は未対応");
        (NodeKind::Unsupported { reason }, Bounds::default())
    } else {
        let rect = Bounds::from(rec.bounds);
        (NodeKind::Layer(Box::new(Pixels { rect, color, alpha })), rect.intersect(canvas))
    };

    Node {
        name: rec.name,
        visible: rec.visible,
        opacity: rec.opacity as f32 / 255.0,
        fill_opacity: rec.fill_opacity as f32 / 255.0,
        blend: BlendMode::from_key(&rec.blend),
        clipped: rec.clipped,
        has_effects: rec.has_effects,
        masks,
        bounds,
        kind,
    }
}

fn push_mask(masks: &mut Vec<Mask>, info: Option<MaskInfo>, channel: Channel) {
    let Some(info) = info else { return };
    if info.disabled || matches!(channel.compression, Compression::Unsupported(_)) {
        return;
    }
    masks.push(Mask {
        rect: info.rect.into(),
        default: info.default,
        invert: info.invert,
        channel,
    });
}
