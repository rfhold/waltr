use waltr_tui::{event::AppEvent, input::InputEvent};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

pub const fn ctrl_x_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Char('x'),
        KeyModifiers::CONTROL,
        KeyEventKind::Press,
    )))
}

pub const fn ctrl_t_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Char('t'),
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

pub const fn shift_enter_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Enter,
        KeyModifiers::SHIFT,
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

pub const fn back_tab_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::BackTab,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

pub const fn home_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Home,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

pub const fn end_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::End,
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

pub const fn left_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Left,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

pub const fn down_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Down,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

pub const fn up_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Up,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

pub const fn page_up_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::PageUp,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

pub const fn page_down_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::PageDown,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

pub const fn scroll_up() -> AppEvent {
    AppEvent::Input(InputEvent::Scroll(1))
}

pub const fn scroll_down() -> AppEvent {
    AppEvent::Input(InputEvent::Scroll(-1))
}

pub const fn left_click(column: u16, row: u16) -> AppEvent {
    AppEvent::Input(InputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }))
}
