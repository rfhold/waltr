//! Runtime input normalization for the terminal UI.

use std::{io, time::Duration};

use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};

/// Input event understood by the app state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputEvent {
    /// A keyboard event from Crossterm.
    Key(KeyEvent),
    /// Net mouse wheel movement; positive moves toward earlier content.
    Scroll(i32),
    /// A mouse event from Crossterm.
    Mouse(MouseEvent),
    /// An operating-system Ctrl-C signal.
    CtrlC,
}

/// Drain all currently available terminal input events without blocking.
///
/// # Errors
///
/// Returns an error if Crossterm cannot poll or read terminal input.
pub fn drain_input_events() -> io::Result<Vec<InputEvent>> {
    let mut events = Vec::new();

    while event::poll(Duration::ZERO)? {
        push_terminal_event(&mut events, event::read()?);
    }

    Ok(events)
}

fn push_terminal_event(events: &mut Vec<InputEvent>, event: Event) {
    match event {
        Event::Key(key) => events.push(normalize_key_event(key)),
        Event::Mouse(mouse) => match mouse_scroll_delta(mouse.kind) {
            Some(delta) => push_scroll_event(events, delta),
            None => events.push(InputEvent::Mouse(mouse)),
        },
        _ => {}
    }
}

fn mouse_scroll_delta(kind: MouseEventKind) -> Option<i32> {
    match kind {
        MouseEventKind::ScrollUp => Some(1),
        MouseEventKind::ScrollDown => Some(-1),
        _ => None,
    }
}

fn push_scroll_event(events: &mut Vec<InputEvent>, delta: i32) {
    if delta == 0 {
        return;
    }

    let remove_last = if let Some(InputEvent::Scroll(current)) = events.last_mut() {
        *current = current.saturating_add(delta);
        *current == 0
    } else {
        events.push(InputEvent::Scroll(delta));
        false
    };

    if remove_last {
        events.pop();
    }
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
    use super::{InputEvent, normalize_key_event, push_terminal_event};
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    };

    #[test]
    fn ctrl_c_keypress_normalizes_to_ctrl_c_input() {
        let key = KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        );

        assert_eq!(normalize_key_event(key), InputEvent::CtrlC);
    }

    #[test]
    fn consecutive_scroll_events_coalesce() {
        let mut events = Vec::new();

        push_terminal_event(&mut events, scroll_event(MouseEventKind::ScrollUp));
        push_terminal_event(&mut events, scroll_event(MouseEventKind::ScrollUp));
        push_terminal_event(&mut events, scroll_event(MouseEventKind::ScrollUp));

        assert_eq!(events, vec![InputEvent::Scroll(3)]);
    }

    #[test]
    fn mixed_scroll_events_net_together() {
        let mut events = Vec::new();

        push_terminal_event(&mut events, scroll_event(MouseEventKind::ScrollUp));
        push_terminal_event(&mut events, scroll_event(MouseEventKind::ScrollUp));
        push_terminal_event(&mut events, scroll_event(MouseEventKind::ScrollDown));

        assert_eq!(events, vec![InputEvent::Scroll(1)]);
    }

    #[test]
    fn non_scroll_events_separate_scroll_batches() {
        let mut events = Vec::new();
        let key =
            KeyEvent::new_with_kind(KeyCode::Char('x'), KeyModifiers::NONE, KeyEventKind::Press);

        push_terminal_event(&mut events, scroll_event(MouseEventKind::ScrollUp));
        push_terminal_event(&mut events, Event::Key(key));
        push_terminal_event(&mut events, scroll_event(MouseEventKind::ScrollDown));

        assert_eq!(
            events,
            vec![
                InputEvent::Scroll(1),
                InputEvent::Key(key),
                InputEvent::Scroll(-1),
            ]
        );
    }

    #[test]
    fn opposing_scroll_events_cancel() {
        let mut events = Vec::new();

        push_terminal_event(&mut events, scroll_event(MouseEventKind::ScrollUp));
        push_terminal_event(&mut events, scroll_event(MouseEventKind::ScrollDown));

        assert!(events.is_empty());
    }

    const fn scroll_event(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    }
}
