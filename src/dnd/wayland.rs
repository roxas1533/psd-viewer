//! Wayland のファイルドロップ受信。
//!
//! winit 0.30 の Wayland 実装はドラッグ&ドロップを扱わないので、winit と同じ接続に
//! 別のイベントキューを作り、`wl_data_device` を自前で受け取る。
//!
//! Hyprland はクライアントの最初の data device にしかドラッグを送らないため、
//! 作成と送信は呼び出し元で同期的に済ませ、受信だけを別スレッドで行う。

use std::collections::HashMap;
use std::ffi::{OsString, c_void};
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use eframe::egui;
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::{wl_data_device, wl_data_offer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};

use super::DropEvent;

const URI_LIST: &str = "text/uri-list";

pub struct Listener {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    /// data device を作ってコンポジタへ送り、受信スレッドを起動する。
    ///
    /// # Safety
    ///
    /// `display` は `stop` を呼ぶまで有効な `wl_display` でなければならない。
    pub unsafe fn start(
        display: NonNull<c_void>,
        tx: Sender<DropEvent>,
        ctx: Arc<OnceLock<egui::Context>>,
    ) -> Result<Listener, String> {
        // SAFETY: 呼び出し元が display の寿命を保証している
        let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).map_err(|e| e.to_string())?;
        let manager: WlDataDeviceManager = globals.bind(&qh, 1..=3, ()).map_err(|e| e.to_string())?;
        let device = manager.get_data_device(&seat, &qh, ());
        conn.flush().map_err(|e| e.to_string())?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                // オブジェクトはスレッドが終わるまで保持する
                let _keep = (globals, seat, manager, device);
                let state = State {
                    tx,
                    ctx,
                    mime_types: HashMap::new(),
                    current: None,
                };
                if let Err(e) = run(queue, state, &stop) {
                    eprintln!("ドラッグ&ドロップの受信が止まりました: {e}");
                }
            })
        };
        Ok(Listener {
            stop,
            thread: Some(thread),
        })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(mut queue: EventQueue<State>, mut state: State, stop: &AtomicBool) -> Result<(), String> {
    while !stop.load(Ordering::Relaxed) {
        queue.dispatch_pending(&mut state).map_err(|e| e.to_string())?;
        queue.flush().map_err(|e| e.to_string())?;
        // winit 側が先にソケットを読むこともあるので、タイムアウト付きで待って定期的に確認する
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut fds = [libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: fds は有効な配列
        if unsafe { libc::poll(fds.as_mut_ptr(), 1, 100) } > 0 {
            guard.read().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

struct State {
    tx: Sender<DropEvent>,
    /// アプリの初期化後に設定される
    ctx: Arc<OnceLock<egui::Context>>,
    /// 受け取ったオファーごとの MIME タイプ
    mime_types: HashMap<ObjectId, Vec<String>>,
    /// ドラッグ中のオファー（受け付けたかどうか）
    current: Option<(wl_data_offer::WlDataOffer, bool)>,
}

impl State {
    fn send(&self, event: DropEvent) {
        let _ = self.tx.send(event);
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }

    fn release(&mut self, offer: wl_data_offer::WlDataOffer) {
        self.mime_types.remove(&offer.id());
        offer.destroy();
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        conn: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_data_device::Event;
        match event {
            Event::DataOffer { id } => {
                state.mime_types.insert(id.id(), Vec::new());
            }
            Event::Enter { serial, id, .. } => {
                if let Some((old, _)) = state.current.take() {
                    state.release(old);
                }
                let Some(offer) = id else { return };
                let accepted = state
                    .mime_types
                    .get(&offer.id())
                    .is_some_and(|types| types.iter().any(|t| t == URI_LIST));
                offer.accept(serial, accepted.then(|| URI_LIST.to_owned()));
                if accepted && offer.version() >= 3 {
                    offer.set_actions(DndAction::Copy, DndAction::Copy);
                }
                state.current = Some((offer, accepted));
                if accepted {
                    state.send(DropEvent::Hover(true));
                }
            }
            Event::Leave => {
                if let Some((offer, _)) = state.current.take() {
                    state.release(offer);
                }
                state.send(DropEvent::Hover(false));
            }
            Event::Drop => {
                if let Some((offer, accepted)) = state.current.take() {
                    state.mime_types.remove(&offer.id());
                    if accepted {
                        // 読み終えたら完了を通知してオファーを破棄する
                        receive_files(offer, conn.clone(), state.tx.clone(), state.ctx.clone());
                    } else {
                        offer.destroy();
                    }
                }
                state.send(DropEvent::Hover(false));
            }
            // クリップボードのオファーは使わない
            Event::Selection { id: Some(offer) } => state.release(offer),
            _ => {}
        }
    }

    event_created_child!(State, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.mime_types.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(_: &mut Self, _: &WlDataDeviceManager, _: <WlDataDeviceManager as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

/// ドロップ元にパス一覧を書き込ませ、別スレッドで読み取る。
///
/// 読み終えたら `finish` を送る。コンポジタ（少なくとも Hyprland）は `finish` を受けるまで
/// ドラッグ中の状態を解除しないので、送らないとポインタが戻るたびにドラッグが再開して見える。
fn receive_files(offer: wl_data_offer::WlDataOffer, conn: Connection, tx: Sender<DropEvent>, ctx: Arc<OnceLock<egui::Context>>) {
    let end = |offer: wl_data_offer::WlDataOffer, conn: &Connection| {
        if offer.version() >= 3 {
            offer.finish();
        }
        offer.destroy();
        let _ = conn.flush();
    };

    let Ok((mut reader, writer)) = std::io::pipe() else {
        end(offer, &conn);
        return;
    };
    offer.receive(URI_LIST.to_owned(), writer.as_fd());
    // fd を送り終えてから閉じないと、読み取り側が終端を検出できない
    let _ = conn.flush();
    drop(writer);

    std::thread::spawn(move || {
        let read = read_with_timeout(&mut reader, READ_TIMEOUT_MS);
        end(offer, &conn);
        if let Some(bytes) = read {
            let files = parse_uri_list(&String::from_utf8_lossy(&bytes));
            if !files.is_empty() {
                let _ = tx.send(DropEvent::Files(files));
                if let Some(ctx) = ctx.get() {
                    ctx.request_repaint();
                }
            }
        }
    });
}

/// ドロップ元が書き込まないまま止まっても、ドラッグを終わらせられるようにする待ち時間
const READ_TIMEOUT_MS: i32 = 2000;

/// パイプを終端まで読む。一定時間データが来なければ打ち切る。
fn read_with_timeout(reader: &mut std::io::PipeReader, timeout_ms: i32) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let mut fds = [libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: fds は有効な配列
        if unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) } <= 0 {
            return None;
        }
        match reader.read(&mut chunk) {
            Ok(0) => return Some(bytes),
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.strip_prefix("file://"))
        .map(|rest| {
            // file://host/path の host 部分を読み飛ばす
            let path = match rest.find('/') {
                Some(i) => &rest[i..],
                None => rest,
            };
            PathBuf::from(OsString::from_vec(percent_decode(path)))
        })
        .collect()
}

fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (bytes.get(i + 1).and_then(|&b| hex(b)), bytes.get(i + 2).and_then(|&b| hex(b)))
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_until_writer_closes() {
        use std::io::Write;
        let (mut reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(b"file:///tmp/a.psd\r\n").unwrap();
        drop(writer);
        assert_eq!(read_with_timeout(&mut reader, 1000).as_deref(), Some(&b"file:///tmp/a.psd\r\n"[..]));
    }

    #[test]
    fn gives_up_when_writer_stalls() {
        let (mut reader, _writer) = std::io::pipe().unwrap();
        let start = std::time::Instant::now();
        assert_eq!(read_with_timeout(&mut reader, 50), None);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn parses_uri_list() {
        let text = "# comment\r\nfile:///home/ro/a%20b.psd\r\nfile://localhost/tmp/%E6%97%A5.psd\r\nhttps://example.com/x\r\n";
        assert_eq!(
            parse_uri_list(text),
            vec![PathBuf::from("/home/ro/a b.psd"), PathBuf::from("/tmp/日.psd")]
        );
    }
}
