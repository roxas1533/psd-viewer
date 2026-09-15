//! 日本語フォントを実行時に読み込む（バイナリに埋め込まない）。

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

/// `PSD_VIEWER_FONT`（`パス` または `パス:インデックス`）、なければ OS の標準的な場所から探す。
pub fn install_japanese_font(ctx: &egui::Context) {
    let Some((path, index)) = from_env().or_else(from_system) else {
        eprintln!("日本語フォントが見つかりません。PSD_VIEWER_FONT でフォントファイルを指定してください");
        return;
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("フォントを読み込めません {}: {e}", path.display());
            return;
        }
    };

    let mut data = FontData::from_owned(bytes);
    data.index = index;

    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("japanese".into(), Arc::new(data));
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("japanese".into());
    }
    ctx.set_fonts(fonts);
}

fn from_env() -> Option<(PathBuf, u32)> {
    let value = std::env::var("PSD_VIEWER_FONT").ok()?;
    let (path, index) = match value.rsplit_once(':') {
        Some((path, index)) if index.parse::<u32>().is_ok() => (path, index.parse().unwrap()),
        _ => (value.as_str(), 0),
    };
    Some((PathBuf::from(path), index))
}

#[cfg(windows)]
fn from_system() -> Option<(PathBuf, u32)> {
    let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    ["YuGothM.ttc", "meiryo.ttc", "msgothic.ttc"]
        .iter()
        .map(|name| root.join("Fonts").join(name))
        .find(|path| path.is_file())
        .map(|path| (path, 0))
}

#[cfg(not(windows))]
fn from_system() -> Option<(PathBuf, u32)> {
    let output = std::process::Command::new("fc-match")
        .args(["-f", "%{file}\n%{index}", "sans-serif:lang=ja"])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let mut lines = text.lines();
    let path = PathBuf::from(lines.next()?);
    let index = lines.next().and_then(|i| i.parse().ok()).unwrap_or(0);
    path.is_file().then_some((path, index))
}
