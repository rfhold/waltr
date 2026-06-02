use waltr_tui::{event::AppEvent, input::InputEvent};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub const fn ctrl_x_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Char('x'),
        KeyModifiers::CONTROL,
        KeyEventKind::Press,
    )))
}

pub const fn char_press(character: char) -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Char(character),
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

pub const fn escape_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Esc,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

pub const fn enter_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Enter,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

pub const fn backspace_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Backspace,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

pub const fn delete_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Delete,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

pub const fn tab_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Tab,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

pub const fn right_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Right,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}
