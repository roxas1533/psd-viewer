//! 読み込みとタイルの合成を UI スレッドの外で行う。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, ColorImage};

use crate::composite::{self, Region};
use crate::document::Document;
use crate::tiles::TileKey;

/// 1 回にまとめて合成するキャンバス画素数の目安。
/// 小さいほど途中経過が早く届き、新しい要求への切り替えも早い。
const BATCH_AREA: usize = 2_000_000;

pub enum Request {
    Open(PathBuf),
    /// 必要なタイルの一覧（優先順）。前の一覧は破棄して置き換える。
    Tiles {
        doc: Arc<Document>,
        visible: Arc<Vec<bool>>,
        generation: u64,
        tiles: Vec<TileKey>,
        /// UI がこの要求を作った時点で受け取り済みの `Response::Tile` の数
        received: u64,
    },
}

pub enum Response {
    Opened {
        path: PathBuf,
        result: Result<Arc<Document>, String>,
    },
    Tile {
        generation: u64,
        key: TileKey,
        image: ColorImage,
    },
    /// 要求されたタイルをすべて送り終えた
    Idle { generation: u64, elapsed: Duration },
}

pub struct Worker {
    tx: Sender<Request>,
    rx: Receiver<Response>,
}

impl Worker {
    pub fn spawn(ctx: egui::Context) -> Worker {
        let (req_tx, req_rx) = channel::<Request>();
        let (res_tx, res_rx) = channel::<Response>();
        std::thread::spawn(move || {
            let _ = run(req_rx, &res_tx, &ctx);
        });
        Worker { tx: req_tx, rx: res_rx }
    }

    pub fn send(&self, req: Request) {
        let _ = self.tx.send(req);
    }

    pub fn try_recv(&self) -> Option<Response> {
        self.rx.try_recv().ok()
    }
}

struct Job {
    doc: Arc<Document>,
    visible: Arc<Vec<bool>>,
    generation: u64,
    queue: VecDeque<TileKey>,
    started: Instant,
}

fn run(rx: Receiver<Request>, tx: &Sender<Response>, ctx: &egui::Context) -> Result<(), ()> {
    let send = |res: Response| {
        tx.send(res).map_err(|_| ())?;
        ctx.request_repaint();
        Ok(())
    };

    let mut job: Option<Job> = None;
    // 世代ごとに、送ったタイルと送信順の番号
    let mut sent: HashMap<TileKey, u64> = HashMap::new();
    let mut sent_generation = 0;
    let mut next_index = 0u64;

    loop {
        // やることがなければ待ち、あれば溜まっている要求だけ取り込む
        let idle = job.as_ref().is_none_or(|j| j.queue.is_empty());
        let mut requests: Vec<Request> = Vec::new();
        if idle {
            requests.push(rx.recv().map_err(|_| ())?);
        }
        requests.extend(rx.try_iter());

        for req in requests {
            match req {
                Request::Open(path) => {
                    job = None;
                    let result = Document::open(&path).map(Arc::new);
                    send(Response::Opened { path, result })?;
                }
                Request::Tiles { doc, visible, generation, tiles, received } => {
                    if generation != sent_generation {
                        sent.clear();
                        sent_generation = generation;
                    }
                    // UI がまだ受け取っていないだけのタイルは作り直さない
                    let queue: VecDeque<TileKey> = tiles
                        .into_iter()
                        .filter(|k| sent.get(k).is_none_or(|&index| index < received))
                        .collect();
                    let started = match &job {
                        Some(j) if !j.queue.is_empty() => j.started,
                        _ => Instant::now(),
                    };
                    job = Some(Job { doc, visible, generation, queue, started });
                }
            }
        }

        let Some(current) = &mut job else { continue };
        if current.queue.is_empty() {
            send(Response::Idle { generation: current.generation, elapsed: current.started.elapsed() })?;
            job = None;
            continue;
        }

        // 優先順に、合計の処理量が目安に達するまでまとめる
        let mut keys = Vec::new();
        let mut regions: Vec<Region> = Vec::new();
        let mut area = 0;
        while let Some(key) = current.queue.front().copied() {
            let region = key.region(&current.doc);
            if !regions.is_empty() && area + region.canvas_area() > BATCH_AREA {
                break;
            }
            current.queue.pop_front();
            area += region.canvas_area();
            keys.push(key);
            regions.push(region);
        }

        let images = composite::render_regions(&current.doc, &current.visible, &regions);
        for (key, image) in keys.into_iter().zip(images) {
            sent.insert(key, next_index);
            next_index += 1;
            send(Response::Tile { generation: current.generation, key, image })?;
        }
        // 次の要求を待つ前に完了を知らせる
        if current.queue.is_empty() {
            send(Response::Idle { generation: current.generation, elapsed: current.started.elapsed() })?;
            job = None;
        }
    }
}
