//! Runtime input normalization for the terminal UI.

use std::{io, time::Duration};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Input event understood by the app state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputEvent {
    /// A keyboard event from Crossterm.
    Key(KeyEvent),
    /// An operating-system Ctrl-C signal.
    CtrlC,
}

/// Drain all currently available keyboard events without blocking.
///
/// # Errors
///
/// Returns an error if Crossterm cannot poll or read terminal input.
pub fn drain_key_events() -> io::Result<Vec<InputEvent>> {
    let mut events = Vec::new();

    while event::poll(Duration::ZERO)? {
        if let Event::Key(key) = event::read()? {
            events.push(normalize_key_event(key));
        }
    }

    Ok(events)
}

fn normalize_key_event(key: KeyEvent) -> InputEvent {
    if key.kind == KeyEventKind::Press
        && key.code == KeyCode::Char('c')
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return InputEvent::CtrlC;
    }

    InputEvent::Key(key)
}

#[cfg(test)]
mod tests {
    use super::{InputEvent, normalize_key_event};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    #[test]
    fn ctrl_c_keypress_normalizes_to_ctrl_c_input() {
        let key = KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        );

        assert_eq!(normalize_key_event(key), InputEvent::CtrlC);
    }
}
