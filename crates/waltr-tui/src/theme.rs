//! Semantic terminal theme roles for Waltr's TUI.

use ratatui::style::{Color, Modifier, Style};

/// Centralized semantic theme for user-facing terminal styles.
pub const THEME: Theme = Theme;

const ACCENT: Color = Color::Rgb(190, 132, 242);
const DIM_ACCENT: Color = Color::Rgb(72, 58, 92);
const TEXT: Color = Color::Rgb(246, 241, 255);
const MUTED_TEXT: Color = Color::DarkGray;
const SUCCESS: Color = Color::Green;
const DANGER: Color = Color::Red;
const CANVAS: Color = Color::Rgb(7, 5, 12);
const SURFACE: Color = Color::Rgb(15, 12, 23);
const PANEL: Color = Color::Rgb(24, 20, 34);
const FOOTER: Color = Color::Rgb(30, 20, 46);
const FOCUS: Color = Color::Cyan;

/// Named status states that use semantic theme roles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusRole {
    /// Work is in progress or an initial stream has not resolved.
    Loading,
    /// The runtime or resource is available.
    Ready,
    /// The runtime or resource reported an error.
    Error,
    /// A write is in progress.
    Saving,
    /// A destructive write is in progress.
    Deleting,
}

/// Waltr's terminal color and style roles.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Theme;

impl Theme {
    /// Brand accent color, preserving Waltr's neon-lavender identity.
    #[must_use]
    pub const fn accent(self) -> Color {
        ACCENT
    }

    /// Dimmed accent color for separators and low-priority chrome.
    #[must_use]
    pub const fn dim_accent(self) -> Color {
        DIM_ACCENT
    }

    /// Primary readable foreground text color.
    #[must_use]
    pub const fn text(self) -> Color {
        TEXT
    }

    /// Secondary readable foreground text color.
    #[must_use]
    pub const fn muted_text(self) -> Color {
        MUTED_TEXT
    }

    /// Positive state color.
    #[must_use]
    pub const fn success(self) -> Color {
        SUCCESS
    }

    /// Error or destructive state color.
    #[must_use]
    pub const fn danger(self) -> Color {
        DANGER
    }

    /// Base app canvas color.
    #[must_use]
    pub const fn canvas(self) -> Color {
        CANVAS
    }

    /// Base surface color.
    #[must_use]
    pub const fn surface(self) -> Color {
        SURFACE
    }

    /// Raised panel color.
    #[must_use]
    pub const fn panel(self) -> Color {
        PANEL
    }

    /// Footer/status strip color.
    #[must_use]
    pub const fn footer(self) -> Color {
        FOOTER
    }

    /// Focused control or actionable hint color.
    #[must_use]
    pub const fn focus(self) -> Color {
        FOCUS
    }

    /// Brand accent style.
    #[must_use]
    pub fn accent_style(self) -> Style {
        Style::default().fg(self.accent())
    }

    /// Bold brand accent style.
    #[must_use]
    pub fn accent_bold(self) -> Style {
        self.accent_style().add_modifier(Modifier::BOLD)
    }

    /// Dimmed accent style.
    #[must_use]
    pub fn dim_accent_style(self) -> Style {
        Style::default().fg(self.dim_accent())
    }

    /// Primary text style.
    #[must_use]
    pub fn text_style(self) -> Style {
        Style::default().fg(self.text())
    }

    /// Muted text style.
    #[must_use]
    pub fn muted_text_style(self) -> Style {
        Style::default().fg(self.muted_text())
    }

    /// Success state style.
    #[must_use]
    pub fn success_style(self) -> Style {
        Style::default().fg(self.success())
    }

    /// Danger state style.
    #[must_use]
    pub fn danger_style(self) -> Style {
        Style::default().fg(self.danger())
    }

    /// App canvas style.
    #[must_use]
    pub fn canvas_style(self) -> Style {
        Style::default().fg(self.text()).bg(self.canvas()).not_dim()
    }

    /// Surface style.
    #[must_use]
    pub fn surface_style(self) -> Style {
        Style::default()
            .fg(self.text())
            .bg(self.surface())
            .not_dim()
    }

    /// Raised panel style.
    #[must_use]
    pub fn panel_style(self) -> Style {
        Style::default().fg(self.text()).bg(self.panel()).not_dim()
    }

    /// Muted text on raised panels.
    #[must_use]
    pub fn panel_muted_style(self) -> Style {
        Style::default()
            .fg(self.muted_text())
            .bg(self.panel())
            .not_dim()
    }

    /// Footer/status strip style.
    #[must_use]
    pub fn footer_style(self) -> Style {
        Style::default()
            .fg(self.muted_text())
            .bg(self.footer())
            .not_dim()
    }

    /// Focused control or actionable hint style.
    #[must_use]
    pub fn focus_style(self) -> Style {
        Style::default().fg(self.focus())
    }

    /// Semantic status color for loading, ready, error, saving, and deleting states.
    #[must_use]
    pub const fn status_color(self, role: StatusRole) -> Color {
        match role {
            StatusRole::Loading => ACCENT,
            StatusRole::Ready => SUCCESS,
            StatusRole::Error | StatusRole::Deleting => DANGER,
            StatusRole::Saving => FOCUS,
        }
    }

    /// Semantic status style for loading, ready, error, saving, and deleting states.
    #[must_use]
    pub fn status_style(self, role: StatusRole) -> Style {
        Style::default().fg(self.status_color(role))
    }
}
