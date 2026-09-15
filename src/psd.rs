//! PSD/PSB の読み取り。
//!
//! ピクセルは展開せず、各チャンネルの位置と RLE の行オフセットだけを持つ。
//! 合成時に必要な行だけ `Channel::read_row` で展開する。

pub struct Psd {
    pub width: u32,
    pub height: u32,
    /// 1 = Grayscale, 3 = RGB, 4 = CMYK
    pub color_mode: u16,
    /// 下から上の順
    pub layers: Vec<LayerRecord>,
    /// 統合画像のチャンネル（レイヤーがない PSD 用）
    pub merged: Vec<Channel>,
    pub warnings: Vec<String>,
}

pub struct LayerRecord {
    pub name: String,
    pub bounds: Rect,
    pub channels: Vec<(i16, Channel)>,
    pub blend: [u8; 4],
    pub opacity: u8,
    pub fill_opacity: u8,
    pub clipped: bool,
    pub visible: bool,
    /// `lsct` の種類。0 = 通常, 1 = 開いたフォルダ, 2 = 閉じたフォルダ, 3 = フォルダの終端
    pub divider: u32,
    pub user_mask: Option<MaskInfo>,
    pub real_mask: Option<MaskInfo>,
    pub has_effects: bool,
    /// 調整レイヤー・塗りつぶしレイヤーの種類
    pub adjustment: Option<&'static str>,
    /// チャンネルデータが失われている
    pub damaged: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub top: i32,
    pub left: i32,
    pub bottom: i32,
    pub right: i32,
}

impl Rect {
    pub fn width(&self) -> usize {
        (self.right - self.left).max(0) as usize
    }

    pub fn height(&self) -> usize {
        (self.bottom - self.top).max(0) as usize
    }
}

#[derive(Clone, Copy)]
pub struct MaskInfo {
    pub rect: Rect,
    pub default: u8,
    pub disabled: bool,
    pub invert: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Raw,
    Rle,
    Unsupported(u16),
}

pub struct Channel {
    pub width: usize,
    pub compression: Compression,
    /// 圧縮方式フィールドの直後
    start: usize,
    end: usize,
    /// RLE の各行の開始位置（`start` からの相対、末尾に終端を含む）
    rows: Vec<u32>,
}

impl Channel {
    /// `row` 行目を `out`（長さ `width`）に展開する。
    pub fn read_row(&self, data: &[u8], row: usize, out: &mut [u8]) {
        let out = &mut out[..self.width];
        match self.compression {
            Compression::Raw => {
                let from = self.start + row * self.width;
                match data.get(from..from + self.width) {
                    Some(src) if from + self.width <= self.end => out.copy_from_slice(src),
                    _ => out.fill(0),
                }
            }
            Compression::Rle => {
                let (Some(&a), Some(&b)) = (self.rows.get(row), self.rows.get(row + 1)) else {
                    out.fill(0);
                    return;
                };
                let src = &data[self.start + a as usize..self.start + b as usize];
                unpack_bits(src, out);
            }
            Compression::Unsupported(_) => out.fill(0),
        }
    }

    fn new(data: &[u8], start: usize, len: usize, width: usize, height: usize, psb: bool) -> Channel {
        let end = (start + len).min(data.len());
        let comp = if len >= 2 && end >= start + 2 {
            u16::from_be_bytes([data[start], data[start + 1]])
        } else {
            0
        };
        let body = start + 2;
        let mut channel = Channel {
            width,
            compression: Compression::Raw,
            start: body,
            end,
            rows: Vec::new(),
        };
        match comp {
            0 => {}
            1 => {
                channel.compression = Compression::Rle;
                channel.rows = rle_rows(data, body, end, height, psb);
            }
            other => channel.compression = Compression::Unsupported(other),
        }
        if width == 0 || height == 0 {
            channel.compression = Compression::Raw;
        }
        channel
    }
}

/// 行ごとのバイト数表から各行の開始位置を求める。
fn rle_rows(data: &[u8], body: usize, end: usize, height: usize, psb: bool) -> Vec<u32> {
    let entry = if psb { 4 } else { 2 };
    let table_len = height * entry;
    let limit = (end - body.min(end)) as u64;
    let mut rows = Vec::with_capacity(height + 1);
    let mut offset = table_len as u64;
    rows.push(offset.min(limit) as u32);
    for i in 0..height {
        let p = body + i * entry;
        let n = match data.get(p..p + entry) {
            Some(b) if psb => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64,
            Some(b) => u16::from_be_bytes([b[0], b[1]]) as u64,
            None => 0,
        };
        offset += n;
        rows.push(offset.min(limit) as u32);
    }
    rows
}

/// PackBits の展開。足りない分は 0 で埋める。
fn unpack_bits(src: &[u8], out: &mut [u8]) {
    let (mut i, mut o) = (0, 0);
    while i < src.len() && o < out.len() {
        let n = src[i] as i8;
        i += 1;
        if n >= 0 {
            let count = (n as usize + 1).min(out.len() - o).min(src.len() - i);
            out[o..o + count].copy_from_slice(&src[i..i + count]);
            i += n as usize + 1;
            o += count;
        } else if n != -128 {
            let count = (1 - n as isize) as usize;
            let count = count.min(out.len() - o);
            let value = src.get(i).copied().unwrap_or(0);
            i += 1;
            out[o..o + count].fill(value);
            o += count;
        }
    }
    out[o..].fill(0);
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    psb: bool,
}

const EOF: &str = "ファイルが途中で終わっています";

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        let slice = self.data.get(self.pos..self.pos + n).ok_or(EOF)?;
        self.pos += n;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        Ok(self.bytes(N)?.try_into().unwrap())
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn i16(&mut self) -> Result<i16, String> {
        Ok(i16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    /// PSD では 4 バイト、PSB では 8 バイトの長さ
    fn len(&mut self) -> Result<usize, String> {
        if self.psb {
            Ok(self.u64()? as usize)
        } else {
            Ok(self.u32()? as usize)
        }
    }

    /// 追加情報ブロックの長さ。PSB では一部のキーだけ 8 バイト。
    fn block_len(&mut self, key: &[u8; 4]) -> Result<usize, String> {
        let long = matches!(
            key,
            b"LMsk" | b"Lr16" | b"Lr32" | b"Layr" | b"Mt16" | b"Mt32" | b"Mtrn" | b"Alph" | b"FMsk" | b"lnk2" | b"FEid" | b"FXid" | b"PxSD"
        );
        if self.psb && long {
            Ok(self.u64()? as usize)
        } else {
            Ok(self.u32()? as usize)
        }
    }

    fn rect(&mut self) -> Result<Rect, String> {
        Ok(Rect {
            top: self.i32()?,
            left: self.i32()?,
            bottom: self.i32()?,
            right: self.i32()?,
        })
    }
}

pub fn parse(data: &[u8]) -> Result<Psd, String> {
    let mut r = Reader { data, pos: 0, psb: false };

    if r.bytes(4)? != b"8BPS" {
        return Err("PSD ファイルではありません".into());
    }
    r.psb = match r.u16()? {
        1 => false,
        2 => true,
        v => return Err(format!("未対応のバージョンです ({v})")),
    };
    r.bytes(6)?;
    let channel_count = r.u16()? as usize;
    let height = r.u32()?;
    let width = r.u32()?;
    let depth = r.u16()?;
    let color_mode = r.u16()?;
    if depth != 8 {
        return Err(format!("8bit 以外は未対応です ({depth}bit)"));
    }
    if !matches!(color_mode, 1 | 3 | 4) {
        return Err(format!("未対応のカラーモードです ({color_mode})"));
    }

    let len = r.u32()? as usize;
    r.bytes(len)?; // カラーモードデータ
    let len = r.u32()? as usize;
    r.bytes(len)?; // イメージリソース

    let section_len = r.len()?;
    let section_start = r.pos;
    let mut section_end = section_start + section_len;
    let mut layers = Vec::new();
    let mut warnings = Vec::new();

    if section_len > 0 {
        let info_len = r.len()?;
        let info_start = r.pos;
        let mut shift = 0;
        if info_len > 0 {
            (layers, shift) = parse_layer_info(&mut r)?;
        }
        // データが欠けたファイルでは、以降のセクションも同じだけずれている
        r.pos = (info_start + info_len).saturating_add_signed(shift);
        section_end = section_end.saturating_add_signed(shift);

        let damaged = layers.iter().filter(|l| l.damaged && l.divider != 3).count();
        if damaged > 0 {
            warnings.push(format!(
                "ファイルの一部が壊れています（約 {:.1} MB のデータが欠落）。{damaged} 個のレイヤーを表示できません",
                shift.unsigned_abs() as f64 / 1_000_000.0
            ));
        }

        // グローバルマスク情報のあとに追加情報ブロックが続く。16/32bit 以外でも Layr にレイヤーが入ることがある。
        if r.pos + 4 <= section_end {
            let len = r.u32()? as usize;
            r.pos += len;
            while layers.is_empty() && r.pos + 12 <= section_end {
                let sig = r.array::<4>()?;
                if &sig != b"8BIM" && &sig != b"8B64" {
                    break;
                }
                let key = r.array::<4>()?;
                let len = r.block_len(&key)?;
                let start = r.pos;
                if &key == b"Layr" {
                    layers = parse_layer_info(&mut r)?.0;
                }
                r.pos = start + len;
                // パディングの扱いが書き出し元によって違うので、次のシグネチャまで最大 3 バイト進める
                for _ in 0..3 {
                    if matches!(r.data.get(r.pos..r.pos + 4), Some(b"8BIM" | b"8B64")) {
                        break;
                    }
                    r.pos += 1;
                }
            }
        }
    }

    let mut merged = Vec::new();
    if layers.is_empty() {
        merged = parse_merged(data, section_end, channel_count, width as usize, height as usize, r.psb);
    }

    Ok(Psd {
        width,
        height,
        color_mode,
        layers,
        merged,
        warnings,
    })
}

/// レイヤー情報を読む。戻り値の 2 つ目は、データの欠落で後続が宣言位置からずれたバイト数。
fn parse_layer_info(r: &mut Reader) -> Result<(Vec<LayerRecord>, isize), String> {
    let count = r.i16()?.unsigned_abs() as usize;
    let mut records = Vec::with_capacity(count);
    let mut channel_lens: Vec<Vec<(i16, usize)>> = Vec::with_capacity(count);

    for _ in 0..count {
        let bounds = r.rect()?;
        let n = r.u16()? as usize;
        let mut chans = Vec::with_capacity(n);
        for _ in 0..n {
            let id = r.i16()?;
            let len = r.len()?;
            chans.push((id, len));
        }
        if r.bytes(4)? != b"8BIM" {
            return Err("レイヤー情報が壊れています".into());
        }
        let blend = r.array::<4>()?;
        let opacity = r.u8()?;
        let clipping = r.u8()?;
        let flags = r.u8()?;
        r.u8()?;

        let extra_len = r.u32()? as usize;
        let extra_end = r.pos + extra_len;

        // マスク
        let mask_len = r.u32()? as usize;
        let mask_end = r.pos + mask_len;
        let (mut user_mask, mut real_mask) = (None, None);
        if mask_len >= 18 {
            let rect = r.rect()?;
            let default = r.u8()?;
            let mflags = r.u8()?;
            user_mask = Some(MaskInfo {
                rect,
                default,
                disabled: mflags & 2 != 0,
                invert: mflags & 4 != 0,
            });
            if mask_len >= 36 {
                let rflags = r.u8()?;
                let rdefault = r.u8()?;
                let rrect = r.rect()?;
                real_mask = Some(MaskInfo {
                    rect: rrect,
                    default: rdefault,
                    disabled: rflags & 2 != 0,
                    invert: rflags & 4 != 0,
                });
            }
        }
        r.pos = mask_end;

        let len = r.u32()? as usize;
        r.bytes(len)?; // ブレンド範囲

        let name_len = r.u8()? as usize;
        let mut name = String::from_utf8_lossy(r.bytes(name_len)?).into_owned();
        r.pos += (4 - (name_len + 1) % 4) % 4;

        let mut record = LayerRecord {
            name: String::new(),
            bounds,
            channels: Vec::new(),
            blend,
            opacity,
            fill_opacity: 255,
            clipped: clipping != 0,
            visible: flags & 2 == 0,
            divider: 0,
            user_mask,
            real_mask,
            has_effects: false,
            adjustment: None,
            damaged: false,
        };

        while r.pos + 12 <= extra_end {
            let sig = r.array::<4>()?;
            if &sig != b"8BIM" && &sig != b"8B64" {
                break;
            }
            let key = r.array::<4>()?;
            let len = r.block_len(&key)?;
            let start = r.pos;
            let mut block = Reader {
                data: r.data.get(start..(start + len).min(extra_end)).unwrap_or(&[]),
                pos: 0,
                psb: r.psb,
            };
            match &key {
                b"luni" => {
                    if let Ok(n) = block.u32() {
                        let units: Vec<u16> = (0..n).map_while(|_| block.u16().ok()).collect();
                        name = String::from_utf16_lossy(&units).trim_end_matches('\0').to_owned();
                    }
                }
                b"lsct" | b"lsdk" => {
                    record.divider = block.u32().unwrap_or(0);
                    if len >= 12 {
                        block.pos += 4;
                        if let Ok(key) = block.array::<4>() {
                            record.blend = key;
                        }
                    }
                }
                b"iOpa" => record.fill_opacity = block.u8().unwrap_or(255),
                b"lrFX" => record.has_effects |= legacy_effects_enabled(block.data),
                b"lfx2" | b"lmfx" => record.has_effects |= descriptor_effects_enabled(block.data),
                key => {
                    if let Some(name) = adjustment_name(key) {
                        record.adjustment = Some(name);
                    }
                }
            }
            r.pos = start + len;
        }
        r.pos = extra_end;

        record.name = name;
        records.push(record);
        channel_lens.push(chans);
    }

    // チャンネルデータはレコードの後ろにまとめて並ぶ
    let mut slots = Vec::new();
    let mut declared = r.pos;
    for (index, (record, chans)) in records.iter().zip(&channel_lens).enumerate() {
        for &(id, len) in chans {
            let rect = match id {
                -2 => record.user_mask.map_or_else(Rect::default, |m| m.rect),
                -3 => record.real_mask.map_or_else(Rect::default, |m| m.rect),
                _ => record.bounds,
            };
            slots.push(Slot {
                record: index,
                id,
                len,
                width: rect.width(),
                height: rect.height(),
                declared,
            });
            declared += len;
        }
    }

    let (starts, shift) = locate_channels(r.data, &slots, r.psb);
    for (slot, start) in slots.iter().zip(starts) {
        let record = &mut records[slot.record];
        match start {
            Some(start) => {
                let channel = Channel::new(r.data, start, slot.len, slot.width, slot.height, r.psb);
                record.channels.push((slot.id, channel));
            }
            None => record.damaged = true,
        }
    }
    r.pos = declared.saturating_add_signed(shift);

    Ok((records, shift))
}

/// 宣言上のチャンネルの位置と大きさ
struct Slot {
    record: usize,
    id: i16,
    len: usize,
    width: usize,
    height: usize,
    declared: usize,
}

impl Slot {
    fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// 各チャンネルの実際の開始位置を決める。
///
/// 通常は宣言どおりに並ぶ。途中のデータが欠けたファイルでは、先頭が正しくない
/// チャンネルが現れた時点で、後続が一貫して読める「ずれ」を探して読み直す。
/// 読めなかったチャンネルは `None`。
fn locate_channels(data: &[u8], slots: &[Slot], psb: bool) -> (Vec<Option<usize>>, isize) {
    let mut starts = vec![None; slots.len()];
    let mut shift = 0isize;
    // 最後に位置を合わせ直したチャンネル。欠落の始まりはこれより前には遡らない。
    let mut segment = 0;
    let mut candidates: Option<Vec<usize>> = None;
    let mut k = 0;

    while k < slots.len() {
        let slot = &slots[k];
        if let Some(pos) = slot.declared.checked_add_signed(shift)
            && header_ok(data, pos, slot, psb)
        {
            starts[k] = Some(pos);
            k += 1;
            continue;
        }

        // 先頭が正しくても、データの途中から欠けていたチャンネルを遡って外す
        for j in (segment..k).rev() {
            match starts[j] {
                Some(pos) if !rows_ok(data, pos, &slots[j], psb) => starts[j] = None,
                _ => break,
            }
        }

        let positions = candidates.get_or_insert_with(|| rle_candidates(data));
        match resync(data, slots, k, positions, psb) {
            Some((next, new_shift)) => {
                k = next;
                segment = next;
                shift = new_shift;
            }
            None => break,
        }
    }
    (starts, shift)
}

/// 先頭の圧縮方式と長さが矛盾しないか（安価な確認）
fn header_ok(data: &[u8], pos: usize, slot: &Slot, psb: bool) -> bool {
    let Some(bytes) = data.get(pos..pos + slot.len) else {
        return false;
    };
    if slot.len < 2 {
        return slot.len == 0;
    }
    match u16::from_be_bytes([bytes[0], bytes[1]]) {
        0 => slot.len == 2 + slot.width * slot.height,
        1 => {
            let entry = if psb { 4 } else { 2 };
            let table = slot.height * entry;
            let Some(expected) = slot.len.checked_sub(2 + table) else {
                return false;
            };
            let mut sum = 0usize;
            for row in bytes[2..2 + table].chunks_exact(entry) {
                sum += if psb {
                    u32::from_be_bytes([row[0], row[1], row[2], row[3]]) as usize
                } else {
                    u16::from_be_bytes([row[0], row[1]]) as usize
                };
                if sum > expected {
                    return false;
                }
            }
            sum == expected
        }
        // ZIP は中身を確かめられないので信用する
        2 | 3 => true,
        _ => false,
    }
}

/// RLE の各行が、表のバイト数ちょうどで幅ぶんに展開できるか（高価な確認）
fn rows_ok(data: &[u8], pos: usize, slot: &Slot, psb: bool) -> bool {
    if slot.is_empty() || data.get(pos..pos + 2) != Some(&[0, 1]) {
        return true;
    }
    let entry = if psb { 4 } else { 2 };
    let mut row_start = pos + 2 + slot.height * entry;
    for row in 0..slot.height {
        let p = pos + 2 + row * entry;
        let n = if psb {
            u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]) as usize
        } else {
            u16::from_be_bytes([data[p], data[p + 1]]) as usize
        };
        let Some(src) = data.get(row_start..row_start + n) else {
            return false;
        };
        let (mut i, mut out) = (0, 0);
        while i < src.len() {
            let c = src[i] as i8;
            i += 1;
            if c >= 0 {
                out += c as usize + 1;
                i += c as usize + 1;
            } else if c != -128 {
                out += (1 - c as isize) as usize;
                i += 1;
            }
        }
        if out != slot.width || i != src.len() {
            return false;
        }
        row_start += n;
    }
    true
}

/// RLE チャンネルの先頭（圧縮方式 1）になりうる位置
fn rle_candidates(data: &[u8]) -> Vec<usize> {
    data.windows(2).enumerate().filter(|(_, w)| *w == [0, 1]).map(|(i, _)| i).collect()
}

/// `from` 以降のチャンネルのうち、実際の位置が見つかり、後続も同じずれで読めるものを探す。
fn resync(data: &[u8], slots: &[Slot], from: usize, positions: &[usize], psb: bool) -> Option<(usize, isize)> {
    const CONFIRM: usize = 4;
    const MAX_TRIES: usize = 64;

    let tries = slots[from..].iter().enumerate().filter(|(_, s)| !s.is_empty()).take(MAX_TRIES);
    for (offset, slot) in tries {
        let index = from + offset;
        for &pos in positions {
            if !header_ok(data, pos, slot, psb) {
                continue;
            }
            let shift = pos as isize - slot.declared as isize;
            // 同じ内容の別レイヤーに一致しただけでないよう、後続もいくつか確かめる
            let confirmed = slots[index + 1..]
                .iter()
                .filter(|s| !s.is_empty())
                .take(CONFIRM)
                .all(|s| s.declared.checked_add_signed(shift).is_some_and(|p| header_ok(data, p, s, psb)));
            if confirmed {
                return Some((index, shift));
            }
        }
    }
    None
}

fn parse_merged(data: &[u8], pos: usize, channels: usize, width: usize, height: usize, psb: bool) -> Vec<Channel> {
    let Some(comp) = data.get(pos..pos + 2).map(|b| u16::from_be_bytes([b[0], b[1]])) else {
        return Vec::new();
    };
    let body = pos + 2;
    let mut result = Vec::with_capacity(channels);
    match comp {
        0 => {
            for c in 0..channels {
                result.push(Channel {
                    width,
                    compression: Compression::Raw,
                    start: body + c * width * height,
                    end: body + (c + 1) * width * height,
                    rows: Vec::new(),
                });
            }
        }
        1 => {
            // 全チャンネル分の行バイト数表のあとにデータが続く
            let entry = if psb { 4 } else { 2 };
            let table_end = body + channels * height * entry;
            let mut offset = table_end;
            for c in 0..channels {
                let table = body + c * height * entry;
                let mut rows = Vec::with_capacity(height + 1);
                let start = offset;
                rows.push(0u32);
                for i in 0..height {
                    let p = table + i * entry;
                    let n = match data.get(p..p + entry) {
                        Some(b) if psb => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize,
                        Some(b) => u16::from_be_bytes([b[0], b[1]]) as usize,
                        None => 0,
                    };
                    offset = (offset + n).min(data.len());
                    rows.push((offset - start) as u32);
                }
                result.push(Channel {
                    width,
                    compression: Compression::Rle,
                    start,
                    end: offset,
                    rows,
                });
            }
        }
        _ => {}
    }
    result
}

/// 旧形式の効果（`lrFX`）のうち、有効なものがあるか。
fn legacy_effects_enabled(block: &[u8]) -> bool {
    let mut r = Reader { data: block, pos: 2, psb: false };
    let Ok(count) = r.u16() else { return false };
    for _ in 0..count {
        let (Ok(_sig), Ok(key), Ok(size)) = (r.array::<4>(), r.array::<4>(), r.u32()) else {
            return false;
        };
        let body = r.pos;
        // 仕様書の各構造での enabled バイトの位置（version の先頭から）
        let enabled_at = match &key {
            b"dsdw" | b"isdw" => Some(38),
            b"oglw" | b"iglw" => Some(30),
            b"bevl" => Some(55),
            b"sofi" => Some(23),
            _ => None,
        };
        if enabled_at.is_some_and(|at| block.get(body + at).is_some_and(|&v| v != 0)) {
            return true;
        }
        r.pos = body + size as usize;
    }
    false
}

/// 記述子形式の効果（`lfx2` / `lmfx`）の全体スイッチが入っているか。
/// 記述子は解釈せず `masterFXSwitch` の真偽値だけを探す。見つからなければ有効とみなす。
fn descriptor_effects_enabled(block: &[u8]) -> bool {
    const KEY: &[u8] = b"masterFXSwitch";
    let Some(at) = block.windows(KEY.len()).position(|w| w == KEY) else {
        return true;
    };
    let rest = &block[at + KEY.len()..];
    match rest.windows(4).position(|w| w == b"bool") {
        Some(i) => rest.get(i + 4).is_none_or(|&v| v != 0),
        None => true,
    }
}

fn adjustment_name(key: &[u8; 4]) -> Option<&'static str> {
    Some(match key {
        b"levl" => "レベル補正",
        b"curv" => "トーンカーブ",
        b"brit" => "明るさ・コントラスト",
        b"hue2" | b"hue " => "色相・彩度",
        b"blnc" => "カラーバランス",
        b"selc" => "特定色域の選択",
        b"mixr" => "チャンネルミキサー",
        b"grdm" => "グラデーションマップ",
        b"phfl" => "レンズフィルター",
        b"expA" => "露光量",
        b"vibA" => "自然な彩度",
        b"thrs" => "2 階調化",
        b"post" => "ポスタリゼーション",
        b"nvrt" => "階調の反転",
        b"blwh" => "白黒",
        b"clrL" => "カラールックアップ",
        b"SoCo" => "べた塗り",
        b"GdFl" => "グラデーション",
        b"PtFl" => "パターン",
        _ => return None,
    })
}
