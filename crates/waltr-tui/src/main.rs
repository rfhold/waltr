//! Waltr TUI entry point.

use std::io;

use waltr_tui::runtime;

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = runtime::run(&mut terminal).await;
    ratatui::restore();
    result
}
