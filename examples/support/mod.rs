//! Formatting helpers shared by the examples: fixed-width numbers, UTC clock
//! strings, eighth-block bars and ANSI color that respects `NO_COLOR`.
//! Nothing in this file is part of the fin-stream API.

#![allow(dead_code)]

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::fmt;

/// 2026-09-24 14:30:00 UTC. Every example starts its synthetic clock here so
/// the output is identical on every run.
pub const SESSION_START_MS: u64 = 1_790_260_200_000;

/// Is stdout an interactive terminal? Examples pace themselves in real time
/// only when it is, and print instantly when piped or redirected.
pub fn live() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::env::var_os("FIN_STREAM_INSTANT").is_none()
}

/// `HH:MM:SS.mmm` in UTC.
pub fn clock_ms(ms: u64) -> String {
    let s = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        (s / 3600) % 24,
        (s / 60) % 60,
        s % 60,
        ms % 1000
    )
}

/// `HH:MM:SS` in UTC.
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60)
}

/// Fixed decimal places, keeping trailing zeros so columns line up.
pub fn fixed(d: Decimal, dp: u32) -> String {
    let mut r = d.round_dp(dp);
    r.rescale(dp);
    r.to_string()
}

/// Fixed decimal places with thousands separators.
pub fn money(d: Decimal, dp: u32) -> String {
    let s = fixed(d, dp);
    let (sign, s) = match s.strip_prefix('-') {
        Some(rest) => ("-", rest.to_owned()),
        None => ("", s),
    };
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let mut out = String::from(sign);
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out
}

/// A horizontal bar of `value / max * width` cells, drawn with eighth blocks.
pub fn bar(value: f64, max: f64, width: usize) -> String {
    if max <= 0.0 || value <= 0.0 {
        return String::new();
    }
    let eighths = ((value / max).min(1.0) * (width * 8) as f64).round() as usize;
    let eighths = eighths.max(1);
    let mut s = "█".repeat(eighths / 8);
    if eighths % 8 != 0 {
        s.push(['▏', '▎', '▍', '▌', '▋', '▊', '▉'][eighths % 8 - 1]);
    }
    s
}

/// Decimal to f64 for drawing only. Never used for arithmetic on prices.
pub fn f(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}

/// Minimal ANSI styling. Methods return a [`Styled`] whose `Display` pads by
/// the visible text, so `{:>10}` lines up whether color is on or off.
#[derive(Clone, Copy)]
pub struct Paint {
    on: bool,
}

impl Paint {
    /// Color when stdout is a terminal, unless `NO_COLOR` is set.
    /// `FORCE_COLOR=1` keeps color when piping.
    pub fn detect() -> Self {
        use std::io::IsTerminal;
        let forced = std::env::var_os("FORCE_COLOR").is_some();
        let disabled = std::env::var_os("NO_COLOR").is_some();
        Self {
            on: forced || (!disabled && std::io::stdout().is_terminal()),
        }
    }
    pub fn enabled(&self) -> bool {
        self.on
    }
    fn wrap(&self, code: &'static str, s: impl Into<String>) -> Styled {
        Styled {
            text: s.into(),
            code: if self.on { Some(code) } else { None },
        }
    }
    /// Ask side, sells, down-ticks.
    pub fn ask(&self, s: impl Into<String>) -> Styled {
        self.wrap("38;5;203", s)
    }
    /// Bid side, buys, up-ticks.
    pub fn bid(&self, s: impl Into<String>) -> Styled {
        self.wrap("38;5;78", s)
    }
    /// Brass: bar closes, warnings.
    pub fn brass(&self, s: impl Into<String>) -> Styled {
        self.wrap("38;5;179", s)
    }
    pub fn dim(&self, s: impl Into<String>) -> Styled {
        self.wrap("38;5;245", s)
    }
    pub fn bold(&self, s: impl Into<String>) -> Styled {
        self.wrap("1", s)
    }
    pub fn plain(&self, s: impl Into<String>) -> Styled {
        Styled {
            text: s.into(),
            code: None,
        }
    }
}

/// Text plus an optional ANSI code.
pub struct Styled {
    text: String,
    code: Option<&'static str>,
}

impl fmt::Display for Styled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let width = f.width().unwrap_or(0);
        let pad = width.saturating_sub(self.text.chars().count());
        let (left, right) = match f.align() {
            Some(fmt::Alignment::Right) => (pad, 0),
            Some(fmt::Alignment::Center) => (pad / 2, pad - pad / 2),
            _ => (0, pad),
        };
        f.write_str(&" ".repeat(left))?;
        match self.code {
            Some(code) => write!(f, "\x1b[{code}m{}\x1b[0m", self.text)?,
            None => f.write_str(&self.text)?,
        }
        f.write_str(&" ".repeat(right))
    }
}
