//! Theme: the default (neutral grey with blue) or one derived from the wallust palette.
//!
//! A wallust palette is rarely usable raw for an interface (in the author's rice it is
//! almost all dark red on black). So only the *hue* is used: the accent comes from the
//! palette's most saturated color, with lightness adjusted to read well on dark, and
//! the surfaces get a slight tint of that hue. The result is always readable.

use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy)]
pub struct Rgb(pub f32, pub f32, pub f32);

pub struct Palette {
    pub colors: Vec<Rgb>,
}

/// Where wallust (or pywal) usually leaves a palette as JSON.
pub fn palette_candidates() -> Vec<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::home().join(".cache"));
    vec![
        cache.join("wallust/colors.json"),
        cache.join("wal/colors.json"),
        // Template from the hollow-wired rice.
        cache.join("wallust/colors-quickshell.json"),
    ]
}

pub fn find_palette() -> Option<PathBuf> {
    palette_candidates().into_iter().find(|p| p.is_file())
}

pub fn mtime(p: &PathBuf) -> Option<SystemTime> {
    fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.trim().trim_start_matches('#');
    // Byte slicing below: anything non-ASCII would split a character and panic.
    if h.len() < 6 || !h.is_ascii() {
        return None;
    }
    let v = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok().map(|b| b as f32 / 255.0);
    Some(Rgb(v(0)?, v(2)?, v(4)?))
}

/// Accepts the flat format ({"color0": ...}) and pywal's ({"colors": {"color0": ...}}).
pub fn load_palette(p: &PathBuf) -> Option<Palette> {
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(p).ok()?).ok()?;
    let src = v.get("colors").filter(|c| c.is_object()).unwrap_or(&v);
    let colors: Vec<Rgb> = (0..16)
        .filter_map(|i| src.get(format!("color{i}")).and_then(|c| c.as_str()).and_then(parse_hex))
        .collect();
    (colors.len() >= 8).then_some(Palette { colors })
}

fn to_hsl(c: Rgb) -> (f32, f32, f32) {
    let (r, g, b) = (c.0, c.1, c.2);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h / 6.0, s, l)
}

fn from_hsl(h: f32, s: f32, l: f32) -> Rgb {
    if s <= 0.0 {
        return Rgb(l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |mut t: f32| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    Rgb(f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0))
}

pub struct Colors {
    pub bg: Rgb,
    pub sidebar: Rgb,
    pub surface: Rgb,
    pub raised: Rgb,
    pub hover: Rgb,
    pub line: Rgb,
    pub text: Rgb,
    pub dim: Rgb,
    pub faint: Rgb,
    pub accent: Rgb,
    pub on_accent: Rgb,
}

pub fn default_colors() -> Colors {
    let hex = |s| parse_hex(s).unwrap();
    Colors {
        bg: hex("#121418"),
        sidebar: hex("#0d0f12"),
        surface: hex("#181b20"),
        raised: hex("#1d2026"),
        hover: hex("#1e2228"),
        line: hex("#24282f"),
        text: hex("#e4e6ea"),
        dim: hex("#8a919b"),
        faint: hex("#5c636d"),
        accent: hex("#4d8eff"),
        on_accent: hex("#ffffff"),
    }
}

/// Accent color: the palette's most saturated one (ignoring near-black and near-white).
fn pick_accent(p: &Palette) -> Option<(f32, f32)> {
    p.colors
        .iter()
        .map(|c| to_hsl(*c))
        .filter(|(_, s, l)| *l > 0.12 && *l < 0.9 && *s > 0.15)
        .max_by(|a, b| (a.1 * (1.0 - (a.2 - 0.5).abs())).total_cmp(&(b.1 * (1.0 - (b.2 - 0.5).abs()))))
        .map(|(h, s, _)| (h, s))
}

pub fn wallust_colors(p: &Palette) -> Colors {
    let Some((h, s)) = pick_accent(p) else { return default_colors() };
    let tint = |l: f32| from_hsl(h, 0.14, l);
    let accent = from_hsl(h, s.clamp(0.55, 0.85), 0.6);
    Colors {
        bg: tint(0.075),
        sidebar: tint(0.055),
        surface: tint(0.10),
        raised: tint(0.125),
        hover: tint(0.135),
        line: tint(0.165),
        text: from_hsl(h, 0.10, 0.91),
        dim: from_hsl(h, 0.08, 0.62),
        faint: from_hsl(h, 0.07, 0.42),
        accent,
        on_accent: if to_hsl(accent).2 > 0.62 { Rgb(0.08, 0.08, 0.08) } else { Rgb(1.0, 1.0, 1.0) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_rice_palette_gives_a_readable_accent() {
        // The rice's real palette: dark reds on black.
        let hexes = ["#000000", "#3A3A3A", "#4B3C3B", "#523C3B", "#523C3B", "#5B3D3B", "#633D3B", "#750804",
                     "#1C1C1C", "#3A3A3A", "#593E3D", "#643E3C", "#643E3C", "#743E3D", "#833F3D", "#750804"];
        let p = Palette { colors: hexes.iter().map(|h| parse_hex(h).unwrap()).collect() };
        let c = wallust_colors(&p);
        let (h, s, l) = to_hsl(c.accent);
        assert!(!(0.05..0.95).contains(&h), "red hue, got {h}");
        assert!(s >= 0.55 && (l - 0.6).abs() < 0.01);
        assert!(to_hsl(c.bg).2 < 0.1 && to_hsl(c.text).2 > 0.85);
    }

    #[test]
    fn bad_hex_is_rejected_without_panicking() {
        assert!(parse_hex("#1é2345").is_none());
        assert!(parse_hex("#12").is_none());
        assert!(parse_hex("#zz0000").is_none());
        assert!(parse_hex("#4d8eff").is_some());
    }

    #[test]
    fn hsl_round_trip() {
        let c = Rgb(0.3, 0.55, 1.0);
        let (h, s, l) = to_hsl(c);
        let d = from_hsl(h, s, l);
        assert!((c.0 - d.0).abs() < 1e-3 && (c.1 - d.1).abs() < 1e-3 && (c.2 - d.2).abs() < 1e-3);
    }
}
