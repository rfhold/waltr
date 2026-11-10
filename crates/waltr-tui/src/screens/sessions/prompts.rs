//! Terminal drafts for authoritative server interactions.
use std::collections::{BTreeMap, BTreeSet};
use ratatui::{Frame, crossterm::event::{KeyCode, KeyEvent, KeyModifiers}, layout::Rect, text::Line, widgets::{Block, Paragraph, Wrap}};
use waltr_client::{ClientCommand, ClientSnapshot, InteractionIdentity, InteractionRequestView as Request, PendingInteractionView, InteractionResponse as Response, InteractionPermissionDecision as Decision, InteractionElicitationAction as Action, InteractionFormValue as Value, InteractionFieldSchema as Schema, InteractionFormField};
use crate::{screens::SectionAction, theme::THEME};
use super::SessionsSection;

pub(super) type InteractionPromptModel = PendingInteractionView;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct PromptDraft {
    identity: Option<InteractionIdentity>,
    request_index: usize,
    row: usize,
    choice: usize,
    selected: BTreeSet<usize>,
    answers: Vec<Vec<String>>,
    text: String,
    fields: BTreeMap<String, String>,
    arrays: BTreeMap<String, Vec<String>>,
    error: Option<String>,
    submitted: bool,
    submitted_error: Option<String>,
    saw_responding: bool,
    composer: bool,
    scroll: u16,
}

impl PromptDraft {
    pub(super) const fn new() -> Self {
        Self { identity: None, request_index: 0, row: 0, choice: 0, selected: BTreeSet::new(), answers: Vec::new(), text: String::new(), fields: BTreeMap::new(), arrays: BTreeMap::new(), error: None, submitted: false, submitted_error: None, saw_responding: false, composer: false, scroll: 0 }
    }
    pub(super) fn clear(&mut self) { *self = Self::new(); }
    pub(super) const fn composer_focused(&self) -> bool { self.composer }
    pub(super) fn reconcile(&mut self, snapshot: &ClientSnapshot, session: Option<&str>) {
        let pending = session.and_then(|s| snapshot.interactions.get(s)).map(|v| v.pending.as_slice()).unwrap_or_default();
        if let Some(identity) = &self.identity {
            if let Some(index) = pending.iter().position(|p| &p.identity == identity) {
                self.request_index = index;
                self.saw_responding |= pending[index].responding;
                if pending[index].error.is_some() && (self.saw_responding || pending[index].error != self.submitted_error) { self.submitted = false; }
                return;
            }
            self.clear();
        }
        if let Some(p) = pending.get(self.request_index.min(pending.len().saturating_sub(1))) { self.sync(p); }
    }
    fn sync(&mut self, p: &PendingInteractionView) {
        if self.identity.as_ref() == Some(&p.identity) { return; }
        let index = self.request_index;
        self.clear(); self.request_index = index; self.identity = Some(p.identity.clone());
        match &p.request {
            Request::Question { questions } => self.answers = vec![Vec::new(); questions.len()],
            Request::Form { fields, .. } => for f in fields {
                match &f.default {
                    Some(Value::String(v)) => { self.fields.insert(f.name.clone(), v.clone()); }
                    Some(Value::Number(v)) => { self.fields.insert(f.name.clone(), v.to_string()); }
                    Some(Value::Boolean(v)) => { self.fields.insert(f.name.clone(), v.to_string()); }
                    Some(Value::Strings(v)) => { self.arrays.insert(f.name.clone(), v.clone()); }
                    None => {}
                }
            },
            _ => {}
        }
    }
}

pub(super) fn current_interaction_prompt(snapshot: Option<&ClientSnapshot>, session: Option<&str>, draft: &PromptDraft) -> Option<InteractionPromptModel> {
    let view = snapshot?.interactions.get(session?)?;
    view.pending.get(draft.request_index.min(view.pending.len().saturating_sub(1))).cloned()
}

impl SessionsSection {
    pub(super) fn handle_prompt_key(&mut self, key: KeyEvent, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        let Some(snapshot) = snapshot else { return SectionAction::Continue; };
        let Some(session) = self.detail_session_id().map(str::to_owned) else { return SectionAction::Continue; };
        self.prompt_draft.reconcile(snapshot, Some(&session));
        let Some(view) = snapshot.interactions.get(&session) else { return SectionAction::Continue; };
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Left | KeyCode::Right) {
            let next = if key.code == KeyCode::Left { self.prompt_draft.request_index.saturating_sub(1) } else { (self.prompt_draft.request_index + 1).min(view.pending.len().saturating_sub(1)) };
            self.prompt_draft.clear(); self.prompt_draft.request_index = next;
            if let Some(p) = view.pending.get(next) { self.prompt_draft.sync(p); }
            return SectionAction::Continue;
        }
        if key.code == KeyCode::F(6) { self.prompt_draft.composer = !self.prompt_draft.composer; return SectionAction::Continue; }
        if self.prompt_draft.composer {
            return match key.code {
                KeyCode::PageUp => self.page_detail_up(Some(snapshot), None),
                KeyCode::PageDown => self.page_detail_down(Some(snapshot), None),
                _ => self.handle_composer_key(key, Some(snapshot)),
            };
        }
        if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            self.prompt_draft.scroll = if key.code == KeyCode::PageUp { self.prompt_draft.scroll.saturating_sub(5) } else { self.prompt_draft.scroll.saturating_add(5) };
            return SectionAction::Continue;
        }
        let Some(p) = view.pending.get(self.prompt_draft.request_index) else { return SectionAction::Continue; };
        let d = &mut self.prompt_draft;
        if !view.connected || p.responding || d.submitted { return SectionAction::Continue; }
        let mut response = None;
        let submit = key.code == KeyCode::F(7) || (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL));
        let decline = key.code == KeyCode::F(8);
        let cancel = key.code == KeyCode::F(9);
        match &p.request {
            Request::Permission { suggestions, .. } => {
                let plain = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                let decision = match key.code {
                    KeyCode::Char('a') if plain => Some(Decision::AllowOnce),
                    KeyCode::Char('A') if plain && !d.selected.is_empty() => Some(Decision::AllowAlways),
                    KeyCode::Char('d') if plain => Some(Decision::Deny),
                    KeyCode::Char('c') if plain => Some(Decision::CancelWork),
                    KeyCode::Up => { d.choice = d.choice.saturating_sub(1); None }
                    KeyCode::Down => { d.choice = (d.choice + 1).min(suggestions.len().saturating_sub(1)); None }
                    KeyCode::Char(' ') if d.choice < suggestions.len() => { if !d.selected.remove(&d.choice) { d.selected.insert(d.choice); } None }
                    _ => None,
                };
                response = decision.map(|decision| Response::Permission { decision, suggestion_indices: if decision == Decision::AllowAlways { d.selected.iter().copied().collect() } else { Vec::new() }, message: None });
            }
            Request::Question { questions } => {
                if submit {
                    if d.answers.iter().all(|a| !a.is_empty()) { response = Some(Response::Question { answers: d.answers.clone() }); }
                    else { d.error = Some("Answer every question before submitting".into()); }
                } else if let Some(q) = questions.get(d.row) {
                    match key.code {
                        KeyCode::Tab | KeyCode::BackTab => { d.row = if key.code == KeyCode::BackTab { d.row.saturating_sub(1) } else { (d.row + 1) % questions.len().max(1) }; d.choice = 0; d.text.clear(); }
                        KeyCode::Up => d.choice = d.choice.saturating_sub(1),
                        KeyCode::Down => d.choice = (d.choice + 1).min(q.options.len().saturating_sub(1)),
                        KeyCode::Enter => {
                            let value = if q.custom && !d.text.trim().is_empty() { Some(d.text.trim().to_owned()) } else { q.options.get(d.choice).map(|o| o.label.clone()) };
                            if let Some(value) = value { let a = &mut d.answers[d.row]; if let Some(i) = a.iter().position(|v| v == &value) { a.remove(i); } else { if !q.multiple { a.clear(); } a.push(value); } d.text.clear(); }
                        }
                        KeyCode::Backspace if q.custom => { d.text.pop(); }
                        KeyCode::Char(c) if q.custom && !key.modifiers.contains(KeyModifiers::CONTROL) => d.text.push(c),
                        _ => {}
                    }
                }
            }
            Request::Form { fields, .. } => {
                if decline || cancel { response = Some(Response::Form { action: if decline { Action::Decline } else { Action::Cancel }, values: BTreeMap::new() }); }
                else if submit { match form_values(fields, d) { Ok(values) => response = Some(Response::Form { action: Action::Accept, values }), Err(e) => d.error = Some(e) } }
                else if let Some(f) = fields.get(d.row) {
                    let last_choice = match &f.schema { Schema::String { choices: Some(c), .. } | Schema::StringArray { choices: c, .. } => c.len().saturating_sub(1), _ => 0 };
                    match key.code {
                        KeyCode::Tab | KeyCode::BackTab => { d.row = if key.code == KeyCode::BackTab { d.row.saturating_sub(1) } else { (d.row + 1) % fields.len().max(1) }; d.choice = 0; }
                        KeyCode::Up => d.choice = d.choice.saturating_sub(1),
                        KeyCode::Down => d.choice = d.choice.saturating_add(1).min(last_choice),
                        KeyCode::Delete => { d.fields.remove(&f.name); d.arrays.remove(&f.name); }
                        KeyCode::Char(' ') | KeyCode::Enter if matches!(&f.schema, Schema::Boolean | Schema::StringArray { .. } | Schema::String { choices: Some(_), .. }) => match &f.schema {
                            Schema::Boolean => { let value = d.fields.get(&f.name).is_none_or(|v| v != "true"); d.fields.insert(f.name.clone(), value.to_string()); }
                            Schema::String { choices: Some(choices), .. } => { if let Some(v) = choices.get(d.choice.min(choices.len().saturating_sub(1))) { d.fields.insert(f.name.clone(), v.clone()); } }
                            Schema::StringArray { choices, .. } => { if let Some(v) = choices.get(d.choice.min(choices.len().saturating_sub(1))) { let values = d.arrays.entry(f.name.clone()).or_default(); if let Some(i) = values.iter().position(|s| s == v) { values.remove(i); } else { values.push(v.clone()); } } }
                            _ => {}
                        },
                        KeyCode::Backspace => { d.fields.entry(f.name.clone()).or_default().pop(); }
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) && matches!(&f.schema, Schema::String { choices: None, .. } | Schema::Number { .. }) => d.fields.entry(f.name.clone()).or_default().push(c),
                        _ => {}
                    }
                }
            }
            Request::UrlConsent { .. } => if submit || decline || cancel { response = Some(Response::UrlConsent { action: if decline { Action::Decline } else if cancel { Action::Cancel } else { Action::Accept } }); },
            Request::Unsupported { form, .. } => if *form && (decline || cancel) { response = Some(Response::Form { action: if decline { Action::Decline } else { Action::Cancel }, values: BTreeMap::new() }); },
        }
        if let Some(response) = response {
            d.submitted = true; d.submitted_error = p.error.clone(); d.saw_responding = false; d.error = None;
            return SectionAction::ClientCommand(ClientCommand::RespondInteraction { identity: p.identity.clone(), response });
        }
        SectionAction::Continue
    }
}

fn form_values(fields: &[InteractionFormField], d: &PromptDraft) -> Result<BTreeMap<String, Value>, String> {
    let mut values = BTreeMap::new();
    for f in fields {
        let text = d.fields.get(&f.name);
        let invalid = || format!("Invalid value for {}", f.title.as_deref().unwrap_or(&f.name));
        let value = match &f.schema {
            Schema::String { choices, min_length, max_length, format } => text.map(|v| {
                let len = v.chars().count() as u64;
                if choices.as_ref().is_some_and(|c| !c.contains(v)) || min_length.is_some_and(|n| len < n) || max_length.is_some_and(|n| len > n) || !valid_format(v, format.as_deref()) { return Err(invalid()); }
                Ok(Value::String(v.clone()))
            }).transpose()?,
            Schema::Number { integer, minimum, maximum } => text.map(|v| {
                let n = v.parse::<f64>().map_err(|_| invalid())?;
                if !n.is_finite() || (*integer && n.fract() != 0.0) || minimum.is_some_and(|b| n < b) || maximum.is_some_and(|b| n > b) { return Err(invalid()); }
                Ok(Value::Number(n))
            }).transpose()?,
            Schema::Boolean => text.map(|v| v.parse::<bool>().map(Value::Boolean).map_err(|_| invalid())).transpose()?,
            Schema::StringArray { choices, min_items, max_items } => d.arrays.get(&f.name).map(|v| {
                if v.iter().any(|s| !choices.contains(s)) || min_items.is_some_and(|n| (v.len() as u64) < n) || max_items.is_some_and(|n| (v.len() as u64) > n) { return Err(invalid()); }
                Ok(Value::Strings(v.clone()))
            }).transpose()?,
        };
        if let Some(value) = value { values.insert(f.name.clone(), value); } else if f.required { return Err(format!("{} is required", f.title.as_deref().unwrap_or(&f.name))); }
    }
    Ok(values)
}

fn valid_format(value: &str, format: Option<&str>) -> bool {
    match format {
        None => true,
        Some("email") => value.split_once('@').is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && !b.contains('@') && !value.chars().any(char::is_whitespace)),
        Some("uri") => !value.chars().any(char::is_whitespace) && url::Url::parse(value).is_ok(),
        Some("date") => valid_date(value),
        Some("date-time") => value.find(['T', 't']).is_some_and(|i| valid_date(&value[..i]) && valid_time(&value[i + 1..])),
        _ => false,
    }
}
fn valid_date(v: &str) -> bool {
    let p: Vec<_> = v.split('-').collect();
    if p.len() != 3 || p[0].len() != 4 || p[1].len() != 2 || p[2].len() != 2 || p.iter().any(|s| !s.chars().all(|c| c.is_ascii_digit())) { return false; }
    let (Ok(y), Ok(m), Ok(d)) = (p[0].parse::<u32>(), p[1].parse::<u32>(), p[2].parse::<u32>()) else { return false; };
    let days = match m { 2 => if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 29 } else { 28 }, 4 | 6 | 9 | 11 => 30, 1 | 3 | 5 | 7 | 8 | 10 | 12 => 31, _ => 0 }; d > 0 && d <= days
}
fn valid_time(v: &str) -> bool {
    let (clock, zone) = if let Some(clock) = v.strip_suffix('Z').or_else(|| v.strip_suffix('z')) { (clock, None) } else if let Some(i) = v.find(['+', '-']) { (&v[..i], Some(&v[i + 1..])) } else { return false; };
    let p: Vec<_> = clock.split(':').collect();
    if p.len() != 3 { return false; }
    let seconds: Vec<_> = p[2].split('.').collect();
    if seconds.len() > 2 || seconds.get(1).is_some_and(|s| s.is_empty() || !s.chars().all(|c| c.is_ascii_digit())) { return false; }
    let bounded = |s: &str, max: u32| s.len() == 2 && s.chars().all(|c| c.is_ascii_digit()) && s.parse::<u32>().is_ok_and(|v| v <= max);
    bounded(p[0], 23) && bounded(p[1], 59) && bounded(seconds[0], 59) && zone.is_none_or(|z| z.split_once(':').is_some_and(|(h, m)| bounded(h, 23) && bounded(m, 59)))
}

pub(super) fn prompt_height(_: &InteractionPromptModel, _: &PromptDraft) -> u16 { 18 }
pub(super) fn render_prompt(frame: &mut Frame<'_>, area: Rect, p: &InteractionPromptModel, d: &PromptDraft, snapshot: Option<&ClientSnapshot>) {
    let view = snapshot.and_then(|s| s.interactions.get(&p.identity.session_id));
    let mut lines = vec![Line::raw(format!("Request {}/{} · {} · {}", d.request_index + 1, view.map_or(0, |v| v.pending.len()), p.identity.id, p.identity.operation_id)), Line::raw("Ctrl-Left/Right requests · F6 conversation/prompt · PgUp/PgDn prompt scroll")];
    match &p.request {
        Request::Permission { message, identity, claims, suggestions } => {
            lines.push(Line::raw(format!("{message} ({identity})")));
            for claim in claims { lines.push(Line::raw(format!("Claim: {claim}"))); }
            lines.push(Line::raw("a allow once · A grant selected reusable scopes · d deny request · c cancel work"));
            lines.push(Line::raw("Up/Down scope · Space toggle (only server-suggested scopes)"));
            for (i, s) in suggestions.iter().enumerate() { lines.push(Line::raw(format!("{} [{}] {s}", if i == d.choice { ">" } else { " " }, if d.selected.contains(&i) { "x" } else { " " }))); }
        }
        Request::Question { questions } => {
            lines.push(Line::raw("Tab/Shift-Tab question · Up/Down option · Enter toggle/use custom · F7 submit all"));
            for (i, q) in questions.iter().enumerate() {
                lines.push(Line::raw(format!("{} {}: {} [{}]", if d.row == i { ">" } else { " " }, q.header, q.question, if q.multiple { "multiple" } else { "single" })));
                if i == d.row { for (j, o) in q.options.iter().enumerate() { lines.push(Line::raw(format!("{} [{}] {} — {}", if j == d.choice { ">" } else { " " }, if d.answers.get(i).is_some_and(|a| a.contains(&o.label)) { "x" } else { " " }, o.label, o.description))); } if q.custom { lines.push(Line::raw(format!("Type custom answer: {}", d.text))); } }
                lines.push(Line::raw(format!("Answers: {}", d.answers.get(i).map_or(String::new(), |a| a.join(", ")))));
            }
        }
        Request::Form { message, fields } => {
            lines.push(Line::raw(message.clone()));
            lines.push(Line::raw("Tab/Shift-Tab field · type · Up/Down choice · Space/Enter select · Delete omit"));
            lines.push(Line::raw("F7 accept · F8 decline · F9 cancel"));
            for (i, f) in fields.iter().enumerate() {
                lines.push(Line::raw(format!("{} {}{}: {}", if i == d.row { ">" } else { " " }, f.title.as_deref().unwrap_or(&f.name), if f.required { " (required)" } else { "" }, d.fields.get(&f.name).cloned().unwrap_or_else(|| d.arrays.get(&f.name).map_or("(omitted)".into(), |v| v.join(", "))))));
                if let Some(description) = &f.description { lines.push(Line::raw(description.clone())); }
                lines.push(Line::raw(format!("Constraints: {:?}", f.schema)));
                if i == d.row { let choices = match &f.schema { Schema::String { choices: Some(c), .. } | Schema::StringArray { choices: c, .. } => Some(c), _ => None }; if let Some(choices) = choices { for (j, v) in choices.iter().enumerate() { lines.push(Line::raw(format!("{} {} ({v})", if j == d.choice.min(choices.len().saturating_sub(1)) { ">" } else { " " }, f.choice_titles.get(v).unwrap_or(v)))); } } }
            }
        }
        Request::UrlConsent { message, url } => {
            lines.push(Line::raw(message.clone())); lines.push(Line::raw(url.clone()));
            lines.push(Line::raw("Open this full URL manually in your browser if desired. Nothing opens automatically."));
            lines.push(Line::raw("Consent is not browser completion. F7 consent · F8 decline · F9 cancel"));
        }
        Request::Unsupported { message, form } => { lines.push(Line::raw(format!("Unsupported request: {message}"))); lines.push(Line::raw(if *form { "Cannot accept this schema. F8 decline · F9 cancel" } else { "Cannot answer descriptor. Use separate session interrupt if needed." })); }
    }
    if view.is_none_or(|v| !v.connected) { lines.push(Line::raw("Disconnected / observing: responses disabled; work is not cancelled.")); }
    if let Some(e) = view.and_then(|v| v.error.as_ref()) { lines.push(Line::raw(e.clone())); }
    if p.responding || d.submitted { lines.push(Line::raw("Response submitted; awaiting authoritative resolution (no automatic replay).")); }
    if let Some(e) = p.error.as_ref().or(d.error.as_ref()) { lines.push(Line::raw(e.clone())); }
    frame.render_widget(Paragraph::new(lines).style(THEME.text_style()).block(Block::bordered().title(if d.composer { " Interaction (conversation focused) " } else { " Server interaction " })).wrap(Wrap { trim: false }).scroll((d.scroll, 0)), area);
}

#[cfg(test)]
mod tests;
