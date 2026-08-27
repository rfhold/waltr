//! External prompt state, input, parsing, and rendering for session detail routes.

use waltr_client::{
    ClientSnapshot, ExternalRequestView, TimelineContentView, TimelineEventKindView,
    TimelineStatusView,
};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Constraint, Layout, Position, Rect},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};
use serde_json::{Value, json};

use crate::{screens::SectionAction, theme::THEME, ui};

use super::{
    PERMISSION_REQUEST_KIND, QUESTION_REQUEST_KIND, SessionsSection, panel_muted_style,
    panel_style, timeline::accent_border_span,
};

const PROMPT_MAX_HEIGHT: u16 = 14;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct PromptDraft {
    request_id: Option<String>,
    permission_action: PermissionPromptAction,
    deny_message: String,
    deny_message_cursor: usize,
    selected_question: usize,
    selected_option: usize,
    question_focus: QuestionPromptFocus,
    answers: Vec<QuestionAnswerDraft>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QuestionAnswerDraft {
    header: String,
    multiple: bool,
    selections: Vec<String>,
    custom: String,
    custom_cursor: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PermissionPromptAction {
    #[default]
    AllowOnce,
    AllowAlways,
    DenyResume,
    DenyInterrupt,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum QuestionPromptFocus {
    #[default]
    Options,
    Custom,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PermissionPromptModel {
    tool_name: String,
    arguments: String,
    patterns: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QuestionPromptModel {
    questions: Vec<QuestionModel>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QuestionModel {
    header: String,
    question: String,
    multiple: bool,
    custom: bool,
    options: Vec<QuestionOptionModel>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QuestionOptionModel {
    label: String,
    description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ExternalPromptModel {
    item_id: String,
    request: ExternalRequestView,
}

impl PromptDraft {
    pub(super) const fn new() -> Self {
        Self {
            request_id: None,
            permission_action: PermissionPromptAction::AllowOnce,
            deny_message: String::new(),
            deny_message_cursor: 0,
            selected_question: 0,
            selected_option: 0,
            question_focus: QuestionPromptFocus::Options,
            answers: Vec::new(),
        }
    }

    pub(super) fn clear(&mut self) {
        self.request_id = None;
        self.permission_action = PermissionPromptAction::AllowOnce;
        self.deny_message.clear();
        self.deny_message_cursor = 0;
        self.selected_question = 0;
        self.selected_option = 0;
        self.question_focus = QuestionPromptFocus::Options;
        self.answers.clear();
    }

    fn sync_permission_prompt(&mut self, request_id: &str) {
        if self.request_id.as_deref() == Some(request_id) {
            clamp_text_cursor(&self.deny_message, &mut self.deny_message_cursor);
            return;
        }

        self.request_id = Some(request_id.to_owned());
        self.permission_action = PermissionPromptAction::AllowOnce;
        self.deny_message.clear();
        self.deny_message_cursor = 0;
        self.selected_question = 0;
        self.selected_option = 0;
        self.question_focus = QuestionPromptFocus::Options;
        self.answers.clear();
    }

    fn sync_question_prompt(&mut self, request_id: &str, prompt: &QuestionPromptModel) {
        if self.request_id.as_deref() == Some(request_id)
            && self.answers.len() == prompt.questions.len()
        {
            self.clamp_question_selection(prompt);
            return;
        }

        self.request_id = Some(request_id.to_owned());
        self.permission_action = PermissionPromptAction::AllowOnce;
        self.deny_message.clear();
        self.deny_message_cursor = 0;
        self.selected_question = 0;
        self.selected_option = 0;
        self.question_focus = QuestionPromptFocus::Options;
        self.answers = prompt
            .questions
            .iter()
            .map(|question| QuestionAnswerDraft {
                header: question.header.clone(),
                multiple: question.multiple,
                selections: Vec::new(),
                custom: String::new(),
                custom_cursor: 0,
            })
            .collect();
    }

    fn selected_permission_action(&self, request_id: &str) -> PermissionPromptAction {
        if self.request_id.as_deref() == Some(request_id) {
            self.permission_action
        } else {
            PermissionPromptAction::AllowOnce
        }
    }

    fn select_next_permission_action(&mut self) {
        self.permission_action =
            permission_action_at(permission_action_index(self.permission_action) + 1);
    }

    fn select_previous_permission_action(&mut self) {
        let index = permission_action_index(self.permission_action);
        self.permission_action = permission_action_at(index.saturating_sub(1));
    }

    fn permission_deny_message_json(&self) -> Value {
        let message = self.deny_message.trim();
        if message.is_empty() {
            Value::Null
        } else {
            json!(message)
        }
    }

    fn clamp_question_selection(&mut self, prompt: &QuestionPromptModel) {
        let question_max = prompt.questions.len().saturating_sub(1);
        self.selected_question = self.selected_question.min(question_max);
        let Some(question) = prompt.questions.get(self.selected_question) else {
            self.selected_option = 0;
            self.question_focus = QuestionPromptFocus::Options;
            return;
        };

        self.selected_option = self
            .selected_option
            .min(question.options.len().saturating_sub(1));
        if !question.custom {
            self.question_focus = QuestionPromptFocus::Options;
        }
        for answer in &mut self.answers {
            clamp_text_cursor(&answer.custom, &mut answer.custom_cursor);
        }
    }

    fn select_question(&mut self, prompt: &QuestionPromptModel, index: usize) {
        self.selected_question = index.min(prompt.questions.len().saturating_sub(1));
        self.selected_option = 0;
        self.question_focus = QuestionPromptFocus::Options;
    }

    fn select_next_question(&mut self, prompt: &QuestionPromptModel) {
        if prompt.questions.is_empty() {
            return;
        }
        self.select_question(
            prompt,
            (self.selected_question + 1) % prompt.questions.len(),
        );
    }

    fn select_previous_question(&mut self, prompt: &QuestionPromptModel) {
        if prompt.questions.is_empty() {
            return;
        }
        let index = if self.selected_question == 0 {
            prompt.questions.len().saturating_sub(1)
        } else {
            self.selected_question - 1
        };
        self.select_question(prompt, index);
    }

    fn toggle_selected_option(&mut self, prompt: &QuestionPromptModel) {
        let Some(question) = prompt.questions.get(self.selected_question) else {
            return;
        };
        let Some(option) = question.options.get(self.selected_option) else {
            return;
        };
        let Some(answer) = self.answers.get_mut(self.selected_question) else {
            return;
        };

        if answer.multiple {
            if let Some(index) = answer
                .selections
                .iter()
                .position(|selection| selection == &option.label)
            {
                answer.selections.remove(index);
            } else {
                answer.selections.push(option.label.clone());
            }
        } else {
            answer.selections.clear();
            answer.selections.push(option.label.clone());
        }
    }

    fn move_question_focus_down(&mut self, prompt: &QuestionPromptModel) {
        let Some(question) = prompt.questions.get(self.selected_question) else {
            return;
        };

        match self.question_focus {
            QuestionPromptFocus::Options => {
                let max = question.options.len().saturating_sub(1);
                if self.selected_option < max {
                    self.selected_option += 1;
                } else if question.custom {
                    self.question_focus = QuestionPromptFocus::Custom;
                }
            }
            QuestionPromptFocus::Custom => {}
        }
    }

    const fn move_question_focus_up(&mut self) {
        match self.question_focus {
            QuestionPromptFocus::Options => {
                self.selected_option = self.selected_option.saturating_sub(1);
            }
            QuestionPromptFocus::Custom => {
                self.question_focus = QuestionPromptFocus::Options;
            }
        }
    }

    fn handle_custom_key(&mut self, prompt: &QuestionPromptModel, key: KeyEvent) -> TextEditAction {
        let Some(question) = prompt.questions.get(self.selected_question) else {
            return TextEditAction::Unhandled;
        };
        if !question.custom {
            return TextEditAction::Unhandled;
        }
        if let Some(answer) = self.answers.get_mut(self.selected_question) {
            return handle_text_edit_key(&mut answer.custom, &mut answer.custom_cursor, key);
        }
        TextEditAction::Unhandled
    }

    fn question_response_json(&self) -> Value {
        let answers = self
            .answers
            .iter()
            .map(|answer| {
                let mut value = json!({
                    "header": answer.header.clone(),
                    "selections": answer.selections.clone(),
                });
                if !answer.custom.trim().is_empty()
                    && let Some(object) = value.as_object_mut()
                {
                    object.insert("custom".to_owned(), json!(answer.custom.trim()));
                }
                value
            })
            .collect::<Vec<_>>();

        json!({ "answers": answers })
    }
}

impl SessionsSection {
    pub(super) fn handle_prompt_key(
        &mut self,
        key: KeyEvent,
        snapshot: Option<&ClientSnapshot>,
    ) -> SectionAction {
        let Some(session_id) = self.detail_session_id().map(str::to_owned) else {
            return SectionAction::Continue;
        };
        let Some(prompt) = current_external_prompt(snapshot, Some(session_id.as_str())) else {
            return SectionAction::Continue;
        };

        match prompt.request.kind {
            PERMISSION_REQUEST_KIND => self.handle_permission_prompt_key(key, &session_id, &prompt),
            QUESTION_REQUEST_KIND => self.handle_question_prompt_key(key, &session_id, &prompt),
            _ => SectionAction::Continue,
        }
    }

    fn handle_permission_prompt_key(
        &mut self,
        key: KeyEvent,
        session_id: &str,
        prompt: &ExternalPromptModel,
    ) -> SectionAction {
        let permission = permission_prompt_model(&prompt.request);
        self.prompt_draft.sync_permission_prompt(&prompt.request.id);
        let action = self
            .prompt_draft
            .selected_permission_action(&prompt.request.id);

        if action.is_deny()
            && handle_text_edit_key(
                &mut self.prompt_draft.deny_message,
                &mut self.prompt_draft.deny_message_cursor,
                key,
            ) != TextEditAction::Unhandled
        {
            return SectionAction::Continue;
        }

        match key.code {
            KeyCode::Up => {
                self.prompt_draft.select_previous_permission_action();
                SectionAction::Continue
            }
            KeyCode::Down => {
                self.prompt_draft.select_next_permission_action();
                SectionAction::Continue
            }
            KeyCode::Enter => match action {
                PermissionPromptAction::AllowOnce => self.respond_external_request(
                    session_id,
                    &prompt.request.id,
                    json!({ "decision": "allow_once" }),
                ),
                PermissionPromptAction::AllowAlways => self.respond_external_request(
                    session_id,
                    &prompt.request.id,
                    json!({ "decision": "allow_always", "patterns": permission.patterns }),
                ),
                PermissionPromptAction::DenyResume => self.respond_external_request(
                    session_id,
                    &prompt.request.id,
                    json!({
                        "decision": "deny",
                        "message": self.prompt_draft.permission_deny_message_json(),
                        "mode": "return_tool_result"
                    }),
                ),
                PermissionPromptAction::DenyInterrupt => self.deny_permission_and_interrupt(
                    session_id,
                    &prompt.request.id,
                    self.prompt_draft.permission_deny_message_json(),
                ),
            },
            KeyCode::Char('a') if !action.is_deny() => self.respond_external_request(
                session_id,
                &prompt.request.id,
                json!({ "decision": "allow_once" }),
            ),
            KeyCode::Char('A') if !action.is_deny() => self.respond_external_request(
                session_id,
                &prompt.request.id,
                json!({ "decision": "allow_always", "patterns": permission.patterns }),
            ),
            KeyCode::Char('d') if !action.is_deny() => self.respond_external_request(
                session_id,
                &prompt.request.id,
                json!({ "decision": "deny", "message": Value::Null, "mode": "return_tool_result" }),
            ),
            KeyCode::Char('x') if !action.is_deny() => {
                self.deny_permission_and_interrupt(session_id, &prompt.request.id, Value::Null)
            }
            _ => SectionAction::Continue,
        }
    }

    fn handle_question_prompt_key(
        &mut self,
        key: KeyEvent,
        session_id: &str,
        prompt: &ExternalPromptModel,
    ) -> SectionAction {
        let question_prompt = question_prompt_model(&prompt.request);
        self.prompt_draft
            .sync_question_prompt(&prompt.request.id, &question_prompt);

        match key.code {
            KeyCode::Tab => {
                self.prompt_draft.select_next_question(&question_prompt);
                SectionAction::Continue
            }
            KeyCode::BackTab => {
                self.prompt_draft.select_previous_question(&question_prompt);
                SectionAction::Continue
            }
            KeyCode::Up => {
                self.prompt_draft.move_question_focus_up();
                SectionAction::Continue
            }
            KeyCode::Down => {
                self.prompt_draft.move_question_focus_down(&question_prompt);
                SectionAction::Continue
            }
            KeyCode::Left if self.prompt_draft.question_focus == QuestionPromptFocus::Options => {
                self.prompt_draft.select_previous_question(&question_prompt);
                SectionAction::Continue
            }
            KeyCode::Right if self.prompt_draft.question_focus == QuestionPromptFocus::Options => {
                self.prompt_draft.select_next_question(&question_prompt);
                SectionAction::Continue
            }
            KeyCode::Char(' ')
                if self.prompt_draft.question_focus == QuestionPromptFocus::Options =>
            {
                self.prompt_draft.toggle_selected_option(&question_prompt);
                SectionAction::Continue
            }
            KeyCode::Char(character @ '1'..='9')
                if self.prompt_draft.question_focus == QuestionPromptFocus::Options =>
            {
                self.prompt_draft.selected_option = (character as usize - '1' as usize).min(
                    question_prompt
                        .questions
                        .get(self.prompt_draft.selected_question)
                        .map_or(0, |question| question.options.len().saturating_sub(1)),
                );
                self.prompt_draft.toggle_selected_option(&question_prompt);
                SectionAction::Continue
            }
            KeyCode::Enter => self.respond_external_request(
                session_id,
                &prompt.request.id,
                self.prompt_draft.question_response_json(),
            ),
            _ => {
                if self.prompt_draft.question_focus == QuestionPromptFocus::Options
                    && question_prompt
                        .questions
                        .get(self.prompt_draft.selected_question)
                        .is_some_and(|question| question.custom)
                    && matches!(
                        key.code,
                        KeyCode::Char(_)
                            | KeyCode::Backspace
                            | KeyCode::Delete
                            | KeyCode::Home
                            | KeyCode::End
                    )
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    self.prompt_draft.question_focus = QuestionPromptFocus::Custom;
                }
                if self.prompt_draft.handle_custom_key(&question_prompt, key)
                    != TextEditAction::Unhandled
                {
                    return SectionAction::Continue;
                }
                SectionAction::Continue
            }
        }
    }
}

pub(super) fn prompt_height(prompt: &ExternalPromptModel, draft: &PromptDraft) -> u16 {
    let desired = match prompt.request.kind {
        PERMISSION_REQUEST_KIND => {
            let action = draft.selected_permission_action(&prompt.request.id);
            if action.is_deny() { 14 } else { 12 }
        }
        QUESTION_REQUEST_KIND => {
            let question_prompt = question_prompt_model(&prompt.request);
            let Some(question) =
                selected_question_model(&question_prompt, draft, &prompt.request.id)
            else {
                return 6;
            };
            let tab_height = u16::from(question_prompt.questions.len() > 1);
            let custom_height = if question.custom { 3 } else { 0 };
            7 + tab_height
                + u16::try_from(question.options.len()).unwrap_or(PROMPT_MAX_HEIGHT)
                + custom_height
        }
        _ => 6,
    };

    desired.clamp(6, PROMPT_MAX_HEIGHT)
}

pub(super) fn render_prompt(
    frame: &mut Frame<'_>,
    area: Rect,
    prompt: &ExternalPromptModel,
    draft: &PromptDraft,
) {
    frame.render_widget(Block::default().style(panel_style()), area);
    if area.width == 0 || area.height == 0 {
        return;
    }

    let [border_area, inner_area, right_padding_area] = Layout::horizontal([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    let border_lines = (0..border_area.height)
        .map(|_| Line::from(accent_border_span(false)))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(border_lines).style(panel_style()),
        border_area,
    );
    frame.render_widget(
        Paragraph::new(Line::default()).style(panel_style()),
        right_padding_area,
    );

    let [top_padding, content_area, bottom_padding] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(inner_area);
    frame.render_widget(
        Paragraph::new(Line::default()).style(panel_style()),
        top_padding,
    );
    frame.render_widget(
        Paragraph::new(Line::default()).style(panel_style()),
        bottom_padding,
    );

    let content = prompt_render_content(prompt, draft, content_area);
    frame.render_widget(
        Paragraph::new(content.lines).style(panel_style()),
        content_area,
    );
    if let Some(cursor) = content.cursor {
        frame.set_cursor_position(cursor);
    }
}

struct PromptRenderContent {
    lines: Vec<Line<'static>>,
    cursor: Option<Position>,
}

fn prompt_render_content(
    prompt: &ExternalPromptModel,
    draft: &PromptDraft,
    area: Rect,
) -> PromptRenderContent {
    match prompt.request.kind {
        PERMISSION_REQUEST_KIND => permission_prompt_render_content(prompt, draft, area),
        QUESTION_REQUEST_KIND => question_prompt_render_content(prompt, draft, area),
        _ => PromptRenderContent {
            lines: vec![
                Line::from(vec![
                    Span::styled("external request ", THEME.focus_style()),
                    Span::raw(prompt.request.id.clone()),
                ]),
                Line::from(Span::raw(prompt.request.payload_json.clone())),
            ],
            cursor: None,
        },
    }
}

fn permission_prompt_render_content(
    prompt: &ExternalPromptModel,
    draft: &PromptDraft,
    area: Rect,
) -> PromptRenderContent {
    let permission = permission_prompt_model(&prompt.request);
    let action = draft.selected_permission_action(&prompt.request.id);
    let synced = draft.request_id.as_deref() == Some(prompt.request.id.as_str());
    let message = if synced {
        draft.deny_message.as_str()
    } else {
        ""
    };
    let message_cursor = if synced { draft.deny_message_cursor } else { 0 };
    let mut cursor = None;
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Permission required", THEME.accent_bold()),
            Span::raw("  "),
            Span::styled("request ", panel_muted_style()),
            Span::raw(prompt.request.id.clone()),
        ]),
        Line::from(vec![
            Span::styled("tool ", panel_muted_style()),
            Span::raw(permission.tool_name),
        ]),
    ];

    if !permission.arguments.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("args ", panel_muted_style()),
            Span::raw(permission.arguments),
        ]));
    }
    if !permission.patterns.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("patterns ", panel_muted_style()),
            Span::raw(permission.patterns.join(", ")),
        ]));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "Choose an action",
        panel_muted_style(),
    )));
    for candidate in PERMISSION_ACTIONS {
        lines.push(permission_action_line(candidate, candidate == action));
    }

    if action.is_deny() {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            ui::selection_marker(true),
            Span::raw(" deny message "),
            Span::styled("optional", panel_muted_style()),
        ]));
        let input_line = lines.len();
        let prefix_width = 2;
        lines.push(text_input_line(message, "type a reason"));
        cursor = text_cursor_position(area, input_line, prefix_width, message, message_cursor);
    }

    lines.push(Line::from(Span::styled(
        "Up/Down action  Enter confirm  Esc back",
        panel_muted_style(),
    )));
    PromptRenderContent { lines, cursor }
}

fn question_prompt_render_content(
    prompt: &ExternalPromptModel,
    draft: &PromptDraft,
    area: Rect,
) -> PromptRenderContent {
    let question_prompt = question_prompt_model(&prompt.request);
    let synced = draft.request_id.as_deref() == Some(prompt.request.id.as_str());
    let selected_question = if synced {
        draft
            .selected_question
            .min(question_prompt.questions.len().saturating_sub(1))
    } else {
        0
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("Question", THEME.accent_bold()),
        Span::raw("  "),
        Span::styled("request ", panel_muted_style()),
        Span::raw(prompt.request.id.clone()),
    ])];

    if question_prompt.questions.is_empty() {
        lines.push(Line::from("No questions supplied."));
        return PromptRenderContent {
            lines,
            cursor: None,
        };
    }

    if question_prompt.questions.len() > 1 {
        lines.push(question_tabs_line(&question_prompt, selected_question));
    }

    let question = &question_prompt.questions[selected_question];
    let answer = synced
        .then(|| draft.answers.get(selected_question))
        .flatten();
    let focus = if synced {
        draft.question_focus
    } else {
        QuestionPromptFocus::Options
    };
    let selected_option = if synced { draft.selected_option } else { 0 };

    lines.push(Line::from(vec![
        Span::styled(question.header.clone(), THEME.accent_bold()),
        Span::raw("  "),
        Span::raw(question.question.clone()),
    ]));
    lines.push(Line::from(Span::styled(
        if question.multiple {
            "Space toggles options  Tab switches question  Enter submits"
        } else {
            "Space selects option  Tab switches question  Enter submits"
        },
        panel_muted_style(),
    )));

    for (option_index, option) in question.options.iter().enumerate() {
        let checked = answer.is_some_and(|answer| {
            answer
                .selections
                .iter()
                .any(|selection| selection == &option.label)
        });
        let active = focus == QuestionPromptFocus::Options && option_index == selected_option;
        lines.push(question_option_line(option, checked, active));
    }

    let mut cursor = None;
    if question.custom {
        let custom = answer.map_or("", |answer| answer.custom.as_str());
        let custom_cursor = answer.map_or(0, |answer| answer.custom_cursor);
        let active = focus == QuestionPromptFocus::Custom;
        lines.push(Line::default());
        lines.push(Line::from(vec![
            ui::selection_marker(active),
            Span::raw(" custom response "),
            Span::styled("optional", panel_muted_style()),
        ]));
        let input_line = lines.len();
        let prefix_width = 2;
        lines.push(text_input_line(custom, "type a response"));
        if active {
            cursor = text_cursor_position(area, input_line, prefix_width, custom, custom_cursor);
        }
    }

    PromptRenderContent { lines, cursor }
}

fn permission_action_line(action: PermissionPromptAction, selected: bool) -> Line<'static> {
    let label = action.label();
    let style = if selected {
        THEME.accent_bold()
    } else if action.is_deny() {
        THEME.danger_style()
    } else {
        THEME.text_style()
    };

    Line::from(vec![
        ui::selection_marker(selected),
        Span::raw(" "),
        Span::styled(format!("[ {label} ]"), style),
    ])
}

fn question_tabs_line(prompt: &QuestionPromptModel, selected_question: usize) -> Line<'static> {
    let mut spans = vec![Span::styled("questions ", panel_muted_style())];
    for (index, question) in prompt.questions.iter().enumerate() {
        let active = index == selected_question;
        let style = if active {
            THEME.accent_bold()
        } else {
            panel_muted_style()
        };
        spans.push(Span::styled(
            format!(" {} {} ", index + 1, question.header),
            style,
        ));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn question_option_line(
    option: &QuestionOptionModel,
    checked: bool,
    active: bool,
) -> Line<'static> {
    Line::from(vec![
        ui::selection_marker(active),
        Span::raw(if checked { " [x] " } else { " [ ] " }),
        Span::raw(option.label.clone()),
        Span::raw("  "),
        Span::styled(option.description.clone(), panel_muted_style()),
    ])
}

fn text_input_line(text: &str, placeholder: &'static str) -> Line<'static> {
    let value = if text.is_empty() {
        Span::styled(placeholder, panel_muted_style())
    } else {
        Span::styled(text.to_owned(), panel_style())
    };
    Line::from(vec![Span::raw("  "), value])
}

fn text_cursor_position(
    area: Rect,
    line_index: usize,
    prefix_width: u16,
    text: &str,
    cursor: usize,
) -> Option<Position> {
    let line_y = u16::try_from(line_index).ok()?;
    if line_y >= area.height || area.width == 0 {
        return None;
    }

    let cursor = cursor.min(text.len());
    let column = text[..cursor].chars().count();
    let column = u16::try_from(column).unwrap_or(u16::MAX);
    let x = area
        .x
        .saturating_add(prefix_width)
        .saturating_add(column)
        .min(area.right().saturating_sub(1));
    let y = area.y.saturating_add(line_y);
    Some(Position::new(x, y))
}

fn selected_question_model<'a>(
    prompt: &'a QuestionPromptModel,
    draft: &PromptDraft,
    request_id: &str,
) -> Option<&'a QuestionModel> {
    let selected_question = if draft.request_id.as_deref() == Some(request_id) {
        draft
            .selected_question
            .min(prompt.questions.len().saturating_sub(1))
    } else {
        0
    };
    prompt.questions.get(selected_question)
}

const PERMISSION_ACTIONS: [PermissionPromptAction; 4] = [
    PermissionPromptAction::AllowOnce,
    PermissionPromptAction::AllowAlways,
    PermissionPromptAction::DenyResume,
    PermissionPromptAction::DenyInterrupt,
];

impl PermissionPromptAction {
    const fn label(self) -> &'static str {
        match self {
            Self::AllowOnce => "Allow once",
            Self::AllowAlways => "Allow always",
            Self::DenyResume => "Deny and resume",
            Self::DenyInterrupt => "Deny and interrupt",
        }
    }

    const fn is_deny(self) -> bool {
        matches!(self, Self::DenyResume | Self::DenyInterrupt)
    }
}

fn permission_action_index(action: PermissionPromptAction) -> usize {
    PERMISSION_ACTIONS
        .iter()
        .position(|candidate| *candidate == action)
        .unwrap_or(0)
}

fn permission_action_at(index: usize) -> PermissionPromptAction {
    PERMISSION_ACTIONS[index.min(PERMISSION_ACTIONS.len().saturating_sub(1))]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TextEditAction {
    Unhandled,
    Handled,
}

fn handle_text_edit_key(text: &mut String, cursor: &mut usize, key: KeyEvent) -> TextEditAction {
    clamp_text_cursor(text, cursor);
    match key.code {
        KeyCode::Left => {
            move_text_cursor_left(text, cursor);
            TextEditAction::Handled
        }
        KeyCode::Right => {
            move_text_cursor_right(text, cursor);
            TextEditAction::Handled
        }
        KeyCode::Home => {
            *cursor = 0;
            TextEditAction::Handled
        }
        KeyCode::End => {
            *cursor = text.len();
            TextEditAction::Handled
        }
        KeyCode::Backspace => {
            delete_text_before_cursor(text, cursor);
            TextEditAction::Handled
        }
        KeyCode::Delete => {
            delete_text_at_cursor(text, *cursor);
            TextEditAction::Handled
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            text.insert(*cursor, character);
            *cursor += character.len_utf8();
            TextEditAction::Handled
        }
        _ => TextEditAction::Unhandled,
    }
}

fn clamp_text_cursor(text: &str, cursor: &mut usize) {
    *cursor = (*cursor).min(text.len());
    while !text.is_char_boundary(*cursor) {
        *cursor -= 1;
    }
}

fn move_text_cursor_left(text: &str, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    if let Some((previous, _)) = text[..*cursor].char_indices().last() {
        *cursor = previous;
    }
}

fn move_text_cursor_right(text: &str, cursor: &mut usize) {
    if *cursor >= text.len() {
        return;
    }
    if let Some(character) = text[*cursor..].chars().next() {
        *cursor += character.len_utf8();
    }
}

fn delete_text_before_cursor(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let Some((previous, _)) = text[..*cursor].char_indices().last() else {
        return;
    };
    text.drain(previous..*cursor);
    *cursor = previous;
}

fn delete_text_at_cursor(text: &mut String, cursor: usize) {
    if cursor >= text.len() {
        return;
    }
    let end = text[cursor..]
        .chars()
        .next()
        .map_or(cursor, |character| cursor + character.len_utf8());
    text.drain(cursor..end);
}

pub(super) fn current_external_prompt(
    snapshot: Option<&ClientSnapshot>,
    session_id: Option<&str>,
) -> Option<ExternalPromptModel> {
    let timeline = snapshot?.sessions.timelines.get(session_id?)?;
    let mut prompt = None;

    for entry in &timeline.events {
        let Some(event) = entry.durable_event() else {
            continue;
        };
        if event.channel.as_deref() != Some("external_request") {
            continue;
        }

        let resolved = event.kind == TimelineEventKindView::ExternalRequestResolved
            || event.status == Some(TimelineStatusView::Resolved);
        if resolved {
            if prompt
                .as_ref()
                .is_some_and(|prompt: &ExternalPromptModel| prompt.item_id == event.item_id)
            {
                prompt = None;
            }
            continue;
        }

        if let TimelineContentView::ExternalRequest {
            request: Some(request),
        } = &event.content
        {
            prompt = Some(ExternalPromptModel {
                item_id: event.item_id.clone(),
                request: request.clone(),
            });
        }
    }

    prompt
}

fn permission_prompt_model(request: &ExternalRequestView) -> PermissionPromptModel {
    let value = serde_json::from_str::<Value>(&request.payload_json).unwrap_or(Value::Null);
    let tool = value
        .get("tool_call")
        .or_else(|| value.get("toolCall"))
        .unwrap_or(&value);
    let tool_name = json_string(tool.get("name"))
        .or_else(|| json_string(value.get("tool_name")))
        .or_else(|| json_string(value.get("toolName")))
        .or_else(|| json_string(value.get("name")))
        .unwrap_or_else(|| "tool".to_owned());
    let arguments = tool
        .get("arguments")
        .or_else(|| tool.get("arguments_json"))
        .or_else(|| tool.get("argumentsJson"))
        .or_else(|| value.get("arguments"))
        .map(format_json_value)
        .unwrap_or_default();
    let mut patterns =
        value
            .get("patterns")
            .and_then(Value::as_array)
            .map_or_else(Vec::new, |patterns| {
                patterns
                    .iter()
                    .filter_map(|value| json_string(Some(value)))
                    .collect()
            });
    if patterns.is_empty()
        && let Some(pattern) = json_string(value.get("pattern"))
    {
        patterns.push(pattern);
    }

    PermissionPromptModel {
        tool_name,
        arguments,
        patterns,
    }
}

fn question_prompt_model(request: &ExternalRequestView) -> QuestionPromptModel {
    let value = serde_json::from_str::<Value>(&request.payload_json).unwrap_or(Value::Null);
    let questions = value
        .get("questions")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |questions| {
            questions.iter().map(question_model).collect()
        });

    QuestionPromptModel { questions }
}

fn question_model(value: &Value) -> QuestionModel {
    let header = json_string(value.get("header")).unwrap_or_else(|| "Question".to_owned());
    let question = json_string(value.get("question")).unwrap_or_default();
    let multiple = value
        .get("multiple")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let custom = value
        .get("custom")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let options = value
        .get("options")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |options| {
            options.iter().map(question_option_model).collect()
        });

    QuestionModel {
        header,
        question,
        multiple,
        custom,
        options,
    }
}

fn question_option_model(value: &Value) -> QuestionOptionModel {
    if let Some(label) = value.as_str() {
        return QuestionOptionModel {
            label: label.to_owned(),
            description: String::new(),
        };
    }

    QuestionOptionModel {
        label: json_string(value.get("label")).unwrap_or_else(|| "option".to_owned()),
        description: json_string(value.get("description")).unwrap_or_default(),
    }
}

fn json_string(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| {
        value
            .as_str()
            .map(str::to_owned)
            .or_else(|| (!value.is_null()).then(|| value.to_string()))
    })
}

fn format_json_value(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

pub(super) const fn external_request_kind_label(kind: i32) -> &'static str {
    match kind {
        PERMISSION_REQUEST_KIND => "permission request",
        QUESTION_REQUEST_KIND => "question request",
        _ => "external request",
    }
}
