//! Keyboard command mapping for the terminal UI.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// App-level command derived from a key event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyCommand {
    /// No app command is associated with this key.
    None,
    /// Escape was pressed.
    Escape,
    /// The configured leader key was pressed.
    Leader,
    /// A key was pressed while the leader was armed.
    LeaderCommand(LeaderCommand),
}

/// Command entered after the leader key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LeaderCommand {
    /// Open the configuration screen.
    Config,
    /// Open the sessions screen.
    Sessions,
    /// Open provider/model selection for the active session.
    Provider,
    /// Open MCP selection for the active session.
    Mcp,
    /// Quit the app immediately.
    Quit,
    /// Clear in-memory Waltr credentials.
    Logout,
    /// Start deleting the current row/detail item.
    Delete,
    /// Unknown leader command text for notices.
    Unknown(String),
}

/// Classify a key event into an app-level command.
pub fn key_command(key: KeyEvent, leader_armed: bool) -> KeyCommand {
    if key.kind != KeyEventKind::Press {
        return KeyCommand::None;
    }

    if leader_armed {
        return KeyCommand::LeaderCommand(leader_command(key));
    }

    if is_leader_key(key) {
        return KeyCommand::Leader;
    }

    if key.code == KeyCode::Esc {
        return KeyCommand::Escape;
    }

    KeyCommand::None
}

fn leader_command(key: KeyEvent) -> LeaderCommand {
    if matches!(key.code, KeyCode::Char('c')) {
        return LeaderCommand::Config;
    }

    if matches!(key.code, KeyCode::Char('s')) {
        return LeaderCommand::Sessions;
    }

    if matches!(key.code, KeyCode::Char('p')) {
        return LeaderCommand::Provider;
    }

    if matches!(key.code, KeyCode::Char('m')) {
        return LeaderCommand::Mcp;
    }

    if matches!(key.code, KeyCode::Char('q')) {
        return LeaderCommand::Quit;
    }

    if matches!(key.code, KeyCode::Char('l')) {
        return LeaderCommand::Logout;
    }

    if matches!(key.code, KeyCode::Char('d')) {
        return LeaderCommand::Delete;
    }

    LeaderCommand::Unknown(key_name(key))
}

const fn is_leader_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('x')) && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn key_name(key: KeyEvent) -> String {
    match key.code {
        KeyCode::Backspace => "Backspace".to_owned(),
        KeyCode::Enter => "Enter".to_owned(),
        KeyCode::Left => "Left".to_owned(),
        KeyCode::Right => "Right".to_owned(),
        KeyCode::Up => "Up".to_owned(),
        KeyCode::Down => "Down".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        KeyCode::PageUp => "PageUp".to_owned(),
        KeyCode::PageDown => "PageDown".to_owned(),
        KeyCode::Tab => "Tab".to_owned(),
        KeyCode::BackTab => "BackTab".to_owned(),
        KeyCode::Delete => "Delete".to_owned(),
        KeyCode::Insert => "Insert".to_owned(),
        KeyCode::F(number) => format!("F{number}"),
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            format!("Ctrl-{character}")
        }
        KeyCode::Char(character) => character.to_string(),
        KeyCode::Null => "Null".to_owned(),
        KeyCode::Esc => "Esc".to_owned(),
        KeyCode::CapsLock => "CapsLock".to_owned(),
        KeyCode::ScrollLock => "ScrollLock".to_owned(),
        KeyCode::NumLock => "NumLock".to_owned(),
        KeyCode::PrintScreen => "PrintScreen".to_owned(),
        KeyCode::Pause => "Pause".to_owned(),
        KeyCode::Menu => "Menu".to_owned(),
        KeyCode::KeypadBegin => "KeypadBegin".to_owned(),
        KeyCode::Media(media_key) => format!("{media_key:?}"),
        KeyCode::Modifier(modifier_key) => format!("{modifier_key:?}"),
    }
}
