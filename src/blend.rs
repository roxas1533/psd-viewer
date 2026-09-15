//! Photoshop の描画モード。色はすべて 0..1 のストレート値で扱う。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlendMode {
    Normal,
    Darken,
    Multiply,
    ColorBurn,
    LinearBurn,
    DarkerColor,
    Lighten,
    Screen,
    ColorDodge,
    LinearDodge,
    LighterColor,
    Overlay,
    SoftLight,
    HardLight,
    VividLight,
    LinearLight,
    PinLight,
    HardMix,
    Difference,
    Exclusion,
    Subtract,
    Divide,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

impl BlendMode {
    /// PSD のブレンドモードキーから変換する。未対応（ディザ合成など）は通常扱い。
    pub fn from_key(key: &[u8; 4]) -> BlendMode {
        use BlendMode::*;
        match key {
            b"dark" => Darken,
            b"mul " => Multiply,
            b"idiv" => ColorBurn,
            b"lbrn" => LinearBurn,
            b"dkCl" => DarkerColor,
            b"lite" => Lighten,
            b"scrn" => Screen,
            b"div " => ColorDodge,
            b"lddg" => LinearDodge,
            b"lgCl" => LighterColor,
            b"over" => Overlay,
            b"sLit" => SoftLight,
            b"hLit" => HardLight,
            b"vLit" => VividLight,
            b"lLit" => LinearLight,
            b"pLit" => PinLight,
            b"hMix" => HardMix,
            b"diff" => Difference,
            b"smud" => Exclusion,
            b"fsub" => Subtract,
            b"fdiv" => Divide,
            b"hue " => Hue,
            b"sat " => Saturation,
            b"colr" => Color,
            b"lum " => Luminosity,
            _ => Normal,
        }
    }

    /// 背景色 `b` と前景色 `s` から混合結果を求める。
    #[inline]
    pub fn apply(self, b: [f32; 3], s: [f32; 3]) -> [f32; 3] {
        use BlendMode::*;
        match self {
            Normal => s,
            DarkerColor => {
                if lum(s) < lum(b) {
                    s
                } else {
                    b
                }
            }
            LighterColor => {
                if lum(s) > lum(b) {
                    s
                } else {
                    b
                }
            }
            Hue => set_lum(set_sat(s, sat(b)), lum(b)),
            Saturation => set_lum(set_sat(b, sat(s)), lum(b)),
            Color => set_lum(s, lum(b)),
            Luminosity => set_lum(b, lum(s)),
            _ => [0, 1, 2].map(|i| self.separable(b[i], s[i])),
        }
    }

    #[inline]
    fn separable(self, b: f32, s: f32) -> f32 {
        use BlendMode::*;
        match self {
            Darken => b.min(s),
            Multiply => b * s,
            ColorBurn => color_burn(b, s),
            LinearBurn => (b + s - 1.0).max(0.0),
            Lighten => b.max(s),
            Screen => screen(b, s),
            ColorDodge => color_dodge(b, s),
            LinearDodge => (b + s).min(1.0),
            Overlay => hard_light(s, b),
            SoftLight => soft_light(b, s),
            HardLight => hard_light(b, s),
            VividLight => {
                if s <= 0.5 {
                    color_burn(b, 2.0 * s)
                } else {
                    color_dodge(b, 2.0 * s - 1.0)
                }
            }
            LinearLight => (b + 2.0 * s - 1.0).clamp(0.0, 1.0),
            PinLight => {
                if s <= 0.5 {
                    b.min(2.0 * s)
                } else {
                    b.max(2.0 * s - 1.0)
                }
            }
            HardMix => {
                if b + s >= 1.0 {
                    1.0
                } else {
                    0.0
                }
            }
            Difference => (b - s).abs(),
            Exclusion => b + s - 2.0 * b * s,
            Subtract => (b - s).max(0.0),
            Divide => {
                if s <= 0.0 {
                    if b <= 0.0 { 0.0 } else { 1.0 }
                } else {
                    (b / s).min(1.0)
                }
            }
            _ => s,
        }
    }
}

#[inline]
fn screen(b: f32, s: f32) -> f32 {
    b + s - b * s
}

#[inline]
fn hard_light(b: f32, s: f32) -> f32 {
    if s <= 0.5 {
        b * 2.0 * s
    } else {
        screen(b, 2.0 * s - 1.0)
    }
}

#[inline]
fn color_dodge(b: f32, s: f32) -> f32 {
    if b <= 0.0 {
        0.0
    } else if s >= 1.0 {
        1.0
    } else {
        (b / (1.0 - s)).min(1.0)
    }
}

#[inline]
fn color_burn(b: f32, s: f32) -> f32 {
    if b >= 1.0 {
        1.0
    } else if s <= 0.0 {
        0.0
    } else {
        1.0 - ((1.0 - b) / s).min(1.0)
    }
}

#[inline]
fn soft_light(b: f32, s: f32) -> f32 {
    if s <= 0.5 {
        b - (1.0 - 2.0 * s) * b * (1.0 - b)
    } else {
        let d = if b <= 0.25 {
            ((16.0 * b - 12.0) * b + 4.0) * b
        } else {
            b.sqrt()
        };
        b + (2.0 * s - 1.0) * (d - b)
    }
}

// 以下は非分離系モード（W3C Compositing and Blending の定義）

#[inline]
fn lum(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}

#[inline]
fn sat(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}

fn set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - lum(c);
    let c = c.map(|v| v + d);
    let l = lum(c);
    let n = c[0].min(c[1]).min(c[2]);
    let x = c[0].max(c[1]).max(c[2]);
    c.map(|v| {
        let mut v = v;
        if n < 0.0 {
            v = l + (v - l) * l / (l - n);
        }
        if x > 1.0 {
            v = l + (v - l) * (1.0 - l) / (x - l);
        }
        v
    })
}

fn set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
    let max = c[0].max(c[1]).max(c[2]);
    let min = c[0].min(c[1]).min(c[2]);
    if max <= min {
        return [0.0; 3];
    }
    c.map(|v| (v - min) * s / (max - min))
}
