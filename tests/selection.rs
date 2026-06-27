// Verifies that alacritty's selection + selection_to_string works the way
// gridterm relies on: create a Simple selection over a row of text and read
// it back as a String. This catches coordinate/offset mistakes without a GUI.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::Config;
use alacritty_terminal::vte::ansi::Processor;
use alacritty_terminal::Term;

#[derive(Clone)]
struct NoopListener;
impl EventListener for NoopListener {
    fn send_event(&self, _: Event) {}
}

#[test]
fn selection_reads_back_text() {
    let cols = 40;
    let rows = 10;
    let size = TermSize::new(cols, rows);
    let mut term = Term::new(Config::default(), &size, NoopListener);

    // Feed some text into the grid.
    let mut parser: Processor = Processor::new();
    let line = b"COPY_THIS_TEXT_PLEASE";
    parser.advance(&mut term, line);

    // Select the first row from column 0 to the end of the word (inclusive).
    let start = Point::new(Line(0), Column(0));
    let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
    sel.update(Point::new(Line(0), Column(line.len() - 1)), Side::Right);
    term.selection = Some(sel);

    let got = term.selection_to_string();
    assert!(got.is_some(), "selection_to_string returned None");
    let got = got.unwrap();
    assert!(
        got.starts_with("COPY_THIS_TEXT_PLEASE"),
        "unexpected selection text: {got:?}"
    );
}
