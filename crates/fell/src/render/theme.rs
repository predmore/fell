use gpui::{Hsla, Rgba, rgb};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeKind {
    Dark,
    Light,
}

impl ThemeKind {
    pub fn toggle(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    pub fn palette(self) -> Palette {
        match self {
            Self::Dark => Palette::dark(),
            Self::Light => Palette::light(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub background: u32,
    pub foreground: u32,
    pub dimmed: u32,
    pub accent: u32,
    pub quote: u32,
    pub code_bg: u32,
    pub border: u32,
    pub surface: u32,
}

impl Palette {
    fn dark() -> Self {
        Self {
            background: 0x1c1c1c,
            foreground: 0xe8e8e8,
            dimmed: 0xa3a3a3,
            accent: 0x8eb6ff,
            quote: 0x8eb6ff,
            code_bg: 0x2a2a2a,
            border: 0x3a3a3a,
            surface: 0x242424,
        }
    }

    fn light() -> Self {
        Self {
            background: 0xe8e4dc,
            foreground: 0x1a1a1a,
            dimmed: 0x5e5e5e,
            accent: 0x0b4f9c,
            quote: 0x0b4f9c,
            code_bg: 0xe7e5e1,
            border: 0xd5d2cc,
            surface: 0xfffefc,
        }
    }

    pub fn scheme(self) -> &'static str {
        if self.background < 0x808080 {
            "dark"
        } else {
            "light"
        }
    }
}

pub fn paint(hex: u32) -> Rgba {
    rgb(hex)
}

pub fn ink(hex: u32) -> Hsla {
    Hsla::from(rgb(hex))
}
