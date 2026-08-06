use ratatui::{
    style::{Color, Modifier, Style},
    widgets::BorderType,
};

pub(crate) const BG: Color = Color::Rgb(5, 5, 6);
pub(crate) const SURFACE: Color = Color::Rgb(13, 13, 16);
pub(crate) const PANEL: Color = Color::Rgb(21, 21, 24);
pub(crate) const FG: Color = Color::Rgb(244, 244, 245);
pub(crate) const MUTED: Color = Color::Rgb(161, 161, 170);
pub(crate) const DIM: Color = Color::Rgb(113, 113, 122);
pub(crate) const GRID: Color = Color::Rgb(82, 82, 91);
pub(crate) const ACCENT: Color = Color::Rgb(124, 111, 175);
pub(crate) const ACCENT_SOFT: Color = Color::Rgb(141, 127, 192);
pub(crate) const SUCCESS: Color = Color::Rgb(125, 168, 118);
pub(crate) const WARNING: Color = Color::Rgb(245, 158, 11);
pub(crate) const ERROR: Color = Color::Rgb(208, 111, 130);
pub(crate) const INFO: Color = Color::Rgb(56, 189, 248);
pub(crate) const NEUTRAL: Color = Color::Rgb(212, 212, 216);
pub(crate) const CODE_TEXT: Color = Color::Rgb(139, 168, 136);
pub(crate) const USER_BG: Color = Color::Rgb(26, 26, 31);
pub(crate) const TOOL_PENDING_BG: Color = Color::Rgb(16, 16, 21);
pub(crate) const CODE_BG: Color = Color::Rgb(20, 20, 24);

pub(crate) fn active_border() -> Style {
    Style::default().fg(ACCENT)
}

pub(crate) fn muted_border() -> Style {
    Style::default().fg(GRID)
}

pub(crate) fn selected_row() -> Style {
    Style::default()
        .fg(FG)
        .bg(Color::Rgb(34, 34, 38))
        .add_modifier(Modifier::BOLD)
}

pub(crate) const BORDER_TYPE: BorderType = BorderType::Rounded;
