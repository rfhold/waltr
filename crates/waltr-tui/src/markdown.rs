//! Terminal-native Markdown and syntax highlighting helpers.

#![allow(
    clippy::redundant_pub_crate,
    reason = "markdown helpers are intentionally limited to the crate boundary"
)]

use std::sync::LazyLock;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{Style as SyntectStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

use crate::theme::THEME;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Markdown tone matching web session message kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MarkdownTone {
    /// User-authored message.
    User,
    /// Assistant response.
    Assistant,
    /// Assistant thinking/reasoning text.
    Thinking,
}

/// Prefix and base style controls for terminal Markdown rendering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MarkdownRenderOptions {
    /// Message tone.
    pub tone: MarkdownTone,
    /// Prefix for the first rendered line.
    pub first_prefix: &'static str,
    /// Prefix for continuation lines.
    pub rest_prefix: &'static str,
    /// Style used for prefix cells.
    pub prefix_style: Style,
    /// Base content style.
    pub base_style: Style,
}

/// Render Markdown into Ratatui lines.
#[must_use]
pub(crate) fn render_markdown(text: &str, options: MarkdownRenderOptions) -> Vec<Line<'static>> {
    if text.is_empty() {
        return vec![prefixed_line(Vec::new(), &options, 0)];
    }

    let renderer = MarkdownRenderer::new(options);
    renderer.render(text)
}

/// Render a code block using terminal syntax highlighting.
#[must_use]
pub(crate) fn render_code_block(
    code: &str,
    language: Option<&str>,
    prefix: &'static str,
    prefix_style: Style,
    base_style: Style,
) -> Vec<Line<'static>> {
    let language = language.and_then(normalize_language);
    let mut lines = Vec::new();

    let code_lines = highlighted_code_lines(code, language, base_style);
    if code_lines.is_empty() {
        lines.push(Line::from(Span::styled(prefix.to_owned(), prefix_style)));
    } else {
        for code_line in code_lines {
            let mut spans = vec![Span::styled(prefix.to_owned(), prefix_style)];
            spans.extend(code_line);
            lines.push(Line::from(spans));
        }
    }

    lines
}

/// Render one code line into highlighted spans.
#[must_use]
pub(crate) fn highlighted_code_spans(
    code: &str,
    language: Option<&str>,
    base_style: Style,
) -> Vec<Span<'static>> {
    highlighted_code_lines(code, language.and_then(normalize_language), base_style)
        .into_iter()
        .next()
        .unwrap_or_else(|| vec![Span::styled(code.to_owned(), base_style)])
}

/// Normalize a language identifier using the same aliases as the web client.
#[must_use]
pub(crate) fn normalize_language(language: &str) -> Option<&'static str> {
    let language = language.trim().to_ascii_lowercase();
    if language.is_empty() {
        return None;
    }

    Some(match language.as_str() {
        "bash" | "console" | "shell" | "sh" | "terminal" | "zsh" => "bash",
        "diff" | "patch" => "diff",
        "js" | "javascript" => "javascript",
        "jsx" => "jsx",
        "md" | "markdown" => "markdown",
        "rs" | "rust" => "rust",
        "ts" | "typescript" => "typescript",
        "tsx" => "tsx",
        "yml" | "yaml" => "yaml",
        "css" => "css",
        "html" => "html",
        "json" => "json",
        "toml" => "toml",
        _ => return None,
    })
}

/// Infer a highlight language from a file path.
#[must_use]
pub(crate) fn language_from_file_path(path: &str) -> Option<&'static str> {
    let clean_path = path.split(['?', '#']).next().unwrap_or(path);
    let extension = clean_path.rsplit_once('.')?.1;
    normalize_language(extension)
}

struct MarkdownRenderer {
    options: MarkdownRenderOptions,
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<ListState>,
    in_code_block: Option<CodeBlockState>,
    strong_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ListState {
    next: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CodeBlockState {
    language: Option<String>,
    code: String,
}

impl MarkdownRenderer {
    fn new(options: MarkdownRenderOptions) -> Self {
        Self {
            options,
            lines: Vec::new(),
            spans: Vec::new(),
            style_stack: vec![options.base_style],
            list_stack: Vec::new(),
            in_code_block: None,
            strong_index: 0,
        }
    }

    fn render(mut self, text: &str) -> Vec<Line<'static>> {
        let mut markdown_options = Options::empty();
        markdown_options.insert(Options::ENABLE_TABLES);
        markdown_options.insert(Options::ENABLE_STRIKETHROUGH);
        markdown_options.insert(Options::ENABLE_TASKLISTS);

        for event in Parser::new_ext(text, markdown_options) {
            self.handle_event(event);
        }
        self.flush_line();
        self.trim_trailing_blank_lines();

        if self.lines.is_empty() {
            self.lines.push(prefixed_line(Vec::new(), &self.options, 0));
        }

        self.lines
    }

    fn handle_event(&mut self, event: Event<'_>) {
        if let Some(code_block) = &mut self.in_code_block {
            match event {
                Event::Text(text) | Event::Code(text) => code_block.code.push_str(&text),
                Event::SoftBreak | Event::HardBreak => code_block.code.push('\n'),
                Event::End(TagEnd::CodeBlock) => self.end_code_block(),
                _ => {}
            }
            return;
        }

        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(code) => {
                self.push_styled_text(&code, inline_code_style(self.current_style()));
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.flush_line();
                self.push_styled_text("---", THEME.muted_text_style());
                self.flush_line();
            }
            Event::Html(html) | Event::InlineHtml(html) => self.push_text(&html),
            Event::FootnoteReference(reference) => {
                self.push_styled_text(&format!("[{reference}]"), THEME.muted_text_style());
            }
            Event::TaskListMarker(checked) => self.push_text(if checked { "[x] " } else { "[ ] " }),
            Event::InlineMath(math) | Event::DisplayMath(math) => self.push_text(&math),
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.strong_index = 0,
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.push_blank_line();
                self.strong_index = 0;
                self.push_style(heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.push_blank_line();
                self.push_styled_text("> ", THEME.muted_text_style());
                self.push_style(THEME.muted_text_style());
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.push_blank_line();
                self.in_code_block = Some(CodeBlockState {
                    language: code_block_language(kind),
                    code: String::new(),
                });
            }
            Tag::List(start) => {
                self.flush_line();
                self.push_blank_line();
                self.list_stack.push(ListState { next: start });
            }
            Tag::Item => {
                self.flush_line();
                let marker = self.list_stack.last_mut().map_or_else(
                    || "- ".to_owned(),
                    |list| {
                        list.next.as_mut().map_or_else(
                            || "- ".to_owned(),
                            |next| {
                                let marker = format!("{next}. ");
                                *next += 1;
                                marker
                            },
                        )
                    },
                );
                self.push_styled_text(&marker, THEME.muted_text_style());
                self.strong_index = 0;
            }
            Tag::Emphasis => self.push_style(self.current_style().add_modifier(Modifier::ITALIC)),
            Tag::Strong => {
                let style = self.strong_style();
                self.push_style(style);
            }
            Tag::Strikethrough => {
                self.push_style(self.current_style().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { .. } => self.push_style(THEME.focus_style()),
            Tag::Image { dest_url, .. } => {
                self.push_styled_text(&format!("image: {dest_url}"), THEME.muted_text_style());
            }
            Tag::Superscript
            | Tag::Subscript
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::HtmlBlock
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item => {
                self.flush_line();
                if matches!(tag, TagEnd::Heading(_)) {
                    self.pop_style();
                }
                if !matches!(tag, TagEnd::Item) {
                    self.push_blank_line();
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.pop_style();
                self.push_blank_line();
            }
            TagEnd::CodeBlock => self.end_code_block(),
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
                self.push_blank_line();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.pop_style();
            }
            TagEnd::Table => {
                self.flush_line();
                self.push_blank_line();
            }
            TagEnd::TableHead | TagEnd::TableRow => self.flush_line(),
            TagEnd::TableCell => self.push_styled_text(" | ", THEME.muted_text_style()),
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_)
            | TagEnd::HtmlBlock
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::Image => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        self.push_styled_text(text, self.current_style());
    }

    fn push_styled_text(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        self.spans.push(Span::styled(text.to_owned(), style));
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(style);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn current_style(&self) -> Style {
        self.style_stack
            .last()
            .copied()
            .unwrap_or(self.options.base_style)
    }

    fn strong_style(&mut self) -> Style {
        let style = match self.options.tone {
            MarkdownTone::Thinking if self.strong_index == 0 => Style::default()
                .fg(Color::Rgb(214, 158, 46))
                .add_modifier(Modifier::BOLD),
            MarkdownTone::Thinking => Style::default()
                .fg(Color::Rgb(88, 207, 151))
                .add_modifier(Modifier::BOLD),
            MarkdownTone::User | MarkdownTone::Assistant => {
                self.current_style().add_modifier(Modifier::BOLD)
            }
        };
        self.strong_index += 1;
        style
    }

    fn flush_line(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        self.lines
            .push(prefixed_line(spans, &self.options, self.lines.len()));
    }

    fn push_blank_line(&mut self) {
        if self.lines.is_empty() || self.last_line_is_blank() {
            return;
        }

        self.lines
            .push(prefixed_line(Vec::new(), &self.options, self.lines.len()));
    }

    fn trim_trailing_blank_lines(&mut self) {
        while self.last_line_is_blank() {
            self.lines.pop();
        }
    }

    fn last_line_is_blank(&self) -> bool {
        self.lines
            .last()
            .is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
    }

    fn end_code_block(&mut self) {
        let Some(code_block) = self.in_code_block.take() else {
            return;
        };
        self.flush_line();
        let block_lines = render_code_block(
            &code_block.code,
            code_block.language.as_deref(),
            self.options.rest_prefix,
            self.options.prefix_style,
            self.options.base_style,
        );
        self.lines.extend(block_lines);
        self.push_blank_line();
    }
}

fn prefixed_line(
    spans: Vec<Span<'static>>,
    options: &MarkdownRenderOptions,
    line_index: usize,
) -> Line<'static> {
    let prefix = if line_index == 0 {
        options.first_prefix
    } else {
        options.rest_prefix
    };
    let mut prefixed = Vec::with_capacity(spans.len() + 1);
    prefixed.push(Span::styled(prefix.to_owned(), options.prefix_style));
    prefixed.extend(spans);
    Line::from(prefixed)
}

fn code_block_language(kind: CodeBlockKind<'_>) -> Option<String> {
    match kind {
        CodeBlockKind::Indented => None,
        CodeBlockKind::Fenced(language) => language.split_whitespace().next().map(str::to_owned),
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 => THEME.accent_bold(),
        HeadingLevel::H3 | HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => {
            THEME.text_style().add_modifier(Modifier::BOLD)
        }
    }
}

const fn inline_code_style(base_style: Style) -> Style {
    base_style.fg(THEME.accent()).add_modifier(Modifier::BOLD)
}

fn highlighted_code_lines(
    code: &str,
    language: Option<&'static str>,
    base_style: Style,
) -> Vec<Vec<Span<'static>>> {
    let Some(language) = language else {
        return plain_code_lines(code, base_style);
    };
    let syntax = SYNTAX_SET
        .find_syntax_by_token(language)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = highlight_theme();
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();

    for line in LinesWithEndings::from(code) {
        let line = line.trim_end_matches(['\r', '\n']);
        let ranges = highlighter
            .highlight_line(line, &SYNTAX_SET)
            .unwrap_or_else(|_| vec![(SyntectStyle::default(), line)]);
        lines.push(
            ranges
                .into_iter()
                .map(|(style, text)| {
                    Span::styled(text.to_owned(), syntect_style(style, base_style))
                })
                .collect(),
        );
    }

    if lines.is_empty() {
        plain_code_lines(code, base_style)
    } else {
        lines
    }
}

fn plain_code_lines(code: &str, base_style: Style) -> Vec<Vec<Span<'static>>> {
    code.lines()
        .map(|line| vec![Span::styled(line.to_owned(), base_style)])
        .collect()
}

fn highlight_theme() -> &'static Theme {
    THEME_SET
        .themes
        .get("base16-ocean.dark")
        .or_else(|| THEME_SET.themes.get("InspiredGitHub"))
        .or_else(|| THEME_SET.themes.values().next())
        .expect("syntect default themes include at least one theme")
}

const fn syntect_style(style: SyntectStyle, base_style: Style) -> Style {
    base_style.fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_markdown_structure_as_terminal_lines() {
        let lines = render_markdown(
            "# Title\n\nHello **world** and `code`.\n\n- item",
            MarkdownRenderOptions {
                tone: MarkdownTone::Assistant,
                first_prefix: "  ",
                rest_prefix: "  ",
                prefix_style: THEME.text_style(),
                base_style: THEME.text_style(),
            },
        );

        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].spans[1].content.as_ref(), "Title");
        assert!(
            lines[1]
                .spans
                .iter()
                .all(|span| span.content.trim().is_empty())
        );
        assert_eq!(lines[2].spans[2].content.as_ref(), "world");
        assert!(
            lines[2].spans[2]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[2].spans[4].content.as_ref(), "code");
        assert!(
            lines[3]
                .spans
                .iter()
                .all(|span| span.content.trim().is_empty())
        );
        assert_eq!(lines[4].spans[1].content.as_ref(), "- ");
    }

    #[test]
    fn renders_code_blocks_without_language_label() {
        let lines = render_markdown(
            "```rust\nfn main() {}\n```",
            MarkdownRenderOptions {
                tone: MarkdownTone::Assistant,
                first_prefix: "  ",
                rest_prefix: "  ",
                prefix_style: THEME.text_style(),
                base_style: THEME.text_style(),
            },
        );

        assert_eq!(lines.len(), 1);
        assert_ne!(lines[0].spans[1].content.as_ref(), "rust");
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.content.contains("fn"))
        );
    }

    #[test]
    fn normalizes_web_language_aliases() {
        assert_eq!(normalize_language("rs"), Some("rust"));
        assert_eq!(normalize_language("shell"), Some("bash"));
        assert_eq!(normalize_language("patch"), Some("diff"));
        assert_eq!(language_from_file_path("src/app.tsx"), Some("tsx"));
    }

    #[test]
    fn highlights_code_with_terminal_colors() {
        let spans = highlighted_code_spans("fn main() {}", Some("rust"), THEME.text_style());

        assert!(spans.len() > 1);
        assert!(spans.iter().any(|span| span.style.fg != Some(THEME.text())));
    }

    #[test]
    fn thinking_strong_uses_web_like_primary_and_secondary_colors() {
        let lines = render_markdown(
            "**First** and **second**",
            MarkdownRenderOptions {
                tone: MarkdownTone::Thinking,
                first_prefix: "~ ",
                rest_prefix: "  ",
                prefix_style: THEME.muted_text_style(),
                base_style: THEME.muted_text_style(),
            },
        );

        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Rgb(214, 158, 46)));
        assert_eq!(lines[0].spans[3].style.fg, Some(Color::Rgb(88, 207, 151)));
    }
}
