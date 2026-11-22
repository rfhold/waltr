use std::{fmt, path::PathBuf};

use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::Rect,
    text::Line,
    widgets::{Block, Clear, Paragraph},
};
use waltr_client::{MAX_MESSAGE_IMAGE_COUNT, MAX_MESSAGE_TOTAL_IMAGE_BYTES};

use crate::{
    session_images::{Completion, Image, Operation, Request, request_id},
    theme::THEME,
    ui,
};

#[derive(Clone, Default, Eq, PartialEq)]
pub(super) struct Images {
    pub attachments: Vec<Image>,
    pub editor: Option<Editor>,
    pub error: Option<String>,
    pub queued: Option<Request>,
    pending: Option<(u64, bool)>,
    canceling: bool,
}

#[derive(Clone, Default, Eq, PartialEq)]
pub(super) struct Editor {
    path: String,
    selected: usize,
}

impl fmt::Debug for Images {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Images(..)")
    }
}

impl Images {
    pub const fn new() -> Self {
        Self {
            attachments: Vec::new(),
            editor: None,
            error: None,
            queued: None,
            pending: None,
            canceling: false,
        }
    }

    pub const fn pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn owner(&self) -> Option<u64> {
        self.pending.filter(|_| !self.canceling).map(|(id, _)| id)
    }

    pub fn cancel(&mut self) {
        if self.pending() {
            self.canceling = true;
        }
    }

    pub fn send(&mut self, session_id: String, text: String) {
        self.begin(Operation::Send {
            session_id,
            text,
            images: self.attachments.clone(),
        });
    }

    fn begin(&mut self, operation: Operation) {
        let id = request_id();
        self.error = None;
        self.pending = Some((id, matches!(operation, Operation::Send { .. })));
        self.canceling = false;
        self.queued = Some(Request { id, operation });
    }

    pub fn receive(&mut self, id: u64, completion: Completion) -> bool {
        if !self.pending.is_some_and(|(pending_id, _)| pending_id == id) {
            return false;
        }
        self.pending = None;
        let canceled = std::mem::take(&mut self.canceling);
        match completion {
            Completion::Loaded(Ok(image)) if !canceled => {
                if let Some(editor) = &mut self.editor {
                    editor.path.clear();
                    editor.selected = self.attachments.len();
                    self.attachments.push(image);
                }
            }
            Completion::Loaded(Ok(_)) => {}
            Completion::Loaded(Err(error)) => {
                self.error = Some(error);
            }
            Completion::Sent(Err(error), _) => self.error = Some(error.to_string()),
            Completion::Sent(Ok(()), _) => {
                self.attachments.clear();
                return true;
            }
        }
        false
    }

    pub fn editor_key(&mut self, key: KeyEvent, ready: bool) {
        if self.pending() {
            if matches!(key.code, KeyCode::Esc | KeyCode::F(4)) {
                self.cancel();
                if key.code == KeyCode::Esc {
                    self.editor = None;
                }
            }
            return;
        }
        let Some(editor) = &mut self.editor else {
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::F(5) => self.editor = None,
            KeyCode::Enter if !editor.path.is_empty() => {
                if !ready {
                    self.error = Some("The selected session route is not ready".into());
                } else if self.attachments.len() >= MAX_MESSAGE_IMAGE_COUNT {
                    self.error = Some("A message can contain up to 4 images".into());
                } else {
                    let path = PathBuf::from(&editor.path);
                    let remaining = MAX_MESSAGE_TOTAL_IMAGE_BYTES
                        - self.attachments.iter().map(Image::len).sum::<usize>();
                    self.begin(Operation::Load { path, remaining });
                }
            }
            KeyCode::Up => editor.selected = editor.selected.saturating_sub(1),
            KeyCode::Down => {
                editor.selected =
                    (editor.selected + 1).min(self.attachments.len().saturating_sub(1));
            }
            KeyCode::Delete if editor.selected < self.attachments.len() => {
                self.attachments.remove(editor.selected);
                editor.selected = editor
                    .selected
                    .min(self.attachments.len().saturating_sub(1));
                self.error = None;
            }
            KeyCode::Backspace => {
                editor.path.pop();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !ch.is_control()
                    && editor.path.len() + ch.len_utf8() <= 4096 =>
            {
                editor.path.push(ch);
            }
            _ => {}
        }
    }

    pub fn render_hint(&self) -> String {
        if self.canceling {
            return "Canceling image operation…".into();
        }
        match self.pending {
            Some((_, true)) => "Sending message…  F4 Cancel".into(),
            Some((_, false)) => "Reading image…  F4 Cancel".into(),
            None if self.attachments.is_empty() => "F5 Images".into(),
            None => format!("F5 Images ({})", self.attachments.len()),
        }
    }

    pub fn render_editor(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(editor) = &self.editor else {
            return;
        };
        let area = ui::centered_rect(area, 68, 11);
        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .title(" Message images ")
            .style(THEME.panel_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let path_area = Rect { height: 1, ..inner };
        let path = Line::raw(format!("Path: {}", editor.path));
        let visible_width = usize::from(inner.width.saturating_sub(1));
        let scroll = path.width().saturating_sub(visible_width);
        frame.set_cursor_position((
            inner.x + u16::try_from(path.width().min(visible_width)).unwrap_or_default(),
            inner.y,
        ));
        frame.render_widget(
            Paragraph::new(path).scroll((0, u16::try_from(scroll).unwrap_or(u16::MAX))),
            path_area,
        );
        let mut lines = vec![Line::raw(if self.pending() {
            self.render_hint()
        } else {
            "Enter Add  ↑/↓ Select  Delete Remove  Esc Done".into()
        })];
        if self.attachments.is_empty() {
            lines.push(Line::raw("No attached images"));
        }
        for (index, image) in self.attachments.iter().enumerate() {
            lines.push(Line::styled(
                format!("{}  {}", index + 1, image.label()),
                if editor.selected == index {
                    THEME.focus_style()
                } else {
                    THEME.text_style()
                },
            ));
        }
        if let Some(error) = &self.error {
            lines.push(ui::error_line(error));
        }
        frame.render_widget(
            Paragraph::new(lines),
            Rect {
                y: inner.y + 1,
                height: inner.height - 1,
                ..inner
            },
        );
    }
}
