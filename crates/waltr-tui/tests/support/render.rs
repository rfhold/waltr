use waltr_tui::app::App;
use ratatui::{Terminal, backend::TestBackend, widgets::Widget};

pub fn render_app(app: &App) -> String {
    let mut terminal =
        Terminal::new(TestBackend::new(80, 24)).expect("test backend should initialize");

    terminal
        .draw(|frame| app.draw(frame))
        .expect("app should render");

    terminal.backend().to_string()
}

pub fn render_widget(widget: impl Widget) -> String {
    let mut terminal =
        Terminal::new(TestBackend::new(80, 24)).expect("test backend should initialize");

    terminal
        .draw(|frame| frame.render_widget(widget, frame.area()))
        .expect("widget should render");

    terminal.backend().to_string()
}
