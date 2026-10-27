use std::sync::atomic::{AtomicU8, Ordering};

use ratatui::style::Color;

use crate::config::load_theme;

pub(super) struct Palette {
    pub background: Color,
    pub surface: Color,
    pub foreground: Color,
    pub muted: Color,
    pub border: Color,
    pub accent: Color,
    pub added: Color,
    pub removed: Color,
}

const DARK: Palette = Palette {
    background: Color::Rgb(0, 0, 0),
    surface: Color::Rgb(14, 14, 14),
    foreground: Color::Rgb(237, 237, 237),
    muted: Color::Rgb(163, 163, 163),
    border: Color::Rgb(51, 51, 51),
    accent: Color::Rgb(232, 115, 74),
    added: Color::Rgb(62, 245, 140),
    removed: Color::Rgb(255, 69, 69),
};

const LIGHT: Palette = Palette {
    background: Color::Rgb(255, 255, 255),
    surface: Color::Rgb(242, 242, 242),
    foreground: Color::Rgb(26, 26, 26),
    muted: Color::Rgb(64, 64, 64),
    border: Color::Rgb(208, 208, 208),
    accent: Color::Rgb(196, 80, 27),
    added: Color::Rgb(0, 135, 58),
    removed: Color::Rgb(217, 0, 27),
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum Theme {
    Dark,
    Light,
}

impl Theme {
    pub const ALL: [Self; 2] = [Self::Dark, Self::Light];
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
    pub const fn description(self) -> &'static str {
        match self {
            Self::Dark => "dark background with light text",
            Self::Light => "light background with dark text",
        }
    }
    const fn palette(self) -> &'static Palette {
        match self {
            Self::Dark => &DARK,
            Self::Light => &LIGHT,
        }
    }
    fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|theme| theme.name().eq_ignore_ascii_case(name))
    }
}

const DEFAULT: Theme = Theme::Dark;
static SELECTED: AtomicU8 = AtomicU8::new(DEFAULT as u8);

pub(super) fn selected() -> Theme {
    match SELECTED.load(Ordering::Relaxed) {
        value if value == Theme::Light as u8 => Theme::Light,
        _ => DEFAULT,
    }
}

pub(super) fn select(theme: Theme) {
    SELECTED.store(theme as u8, Ordering::Relaxed);
}
pub(super) fn palette() -> &'static Palette {
    selected().palette()
}

pub(crate) fn init() {
    let selected = std::env::var("NONAUT_THEME")
        .ok()
        .or_else(|| load_theme().ok().flatten())
        .as_deref()
        .and_then(Theme::from_name)
        .unwrap_or(DEFAULT);
    select(selected);
}
