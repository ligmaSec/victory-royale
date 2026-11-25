//! Local editor for the outgoing files staged by the hardware test runner.
use std::{env, fs, io, path::Path, process::ExitCode};

use anyhow::{Result, ensure};
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::Line,
    widgets::{Block, BorderType, List, ListItem, ListState, Paragraph, Wrap},
};
use ratatui_textarea::TextArea;

struct File {
    name: String,
    editor: TextArea<'static>,
    included: bool,
    edited: bool,
    newline: &'static str,
}

impl File {
    fn load(name: String) -> Result<Self> {
        ensure!(
            Path::new(&name).file_name().and_then(|n| n.to_str()) == Some(&name),
            "Expected a staged filename"
        );
        let contents = fs::read_to_string(&name)?;
        let newline = if contents.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        // split retains the empty final line, preserving the original terminator.
        let mut editor = TextArea::from(contents.split(newline).map(str::to_owned));
        editor.set_line_number_style(Style::new().dark_gray());
        editor.set_cursor_line_style(Style::new().underlined());
        Ok(Self {
            name,
            editor,
            included: true,
            edited: false,
            newline,
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Files,
    Edit,
    Find,
    Discard,
}

struct Review {
    files: Vec<File>,
    selected: usize,
    mode: Mode,
    query: String,
    no_match: bool,
}

impl Review {
    fn key(&mut self, key: KeyEvent) -> Option<bool> {
        use KeyCode::*;
        if self.mode == Mode::Discard {
            match key.code {
                Char('d' | 'D') => return Some(false),
                Enter | Esc => self.mode = Mode::Files,
                _ => {}
            }
            return None;
        }
        if self.mode == Mode::Find {
            match key.code {
                Esc => self.mode = Mode::Edit,
                Backspace => {
                    self.query.pop();
                    self.no_match = false;
                }
                Char(ch) if !ch.is_control() => {
                    self.query.push(ch);
                    self.no_match = false;
                }
                Enter if !self.query.is_empty() => {
                    let editor = &mut self.files[self.selected].editor;
                    self.no_match = editor
                        .set_search_pattern(regex::escape(&self.query))
                        .is_err()
                        || !editor.search_forward(false);
                    if !self.no_match {
                        self.mode = Mode::Edit;
                    }
                }
                _ => {}
            }
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == Char('s') {
            return Some(true);
        }
        let file = &mut self.files[self.selected];
        if self.mode == Mode::Edit {
            if key.code == Esc {
                self.mode = Mode::Files;
            } else if key.code == Char('f') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.mode = Mode::Find;
                self.query.clear();
                self.no_match = false;
            } else if key.code == KeyCode::F(3) && !self.query.is_empty() {
                file.editor.search_forward(false);
            } else if key.code == Char('z') && key.modifiers.contains(KeyModifiers::CONTROL) {
                file.edited |= file.editor.undo();
            } else {
                file.edited |= file.editor.input(key);
            }
        } else {
            match key.code {
                Up => self.selected = self.selected.saturating_sub(1),
                Down => self.selected = (self.selected + 1).min(self.files.len() - 1),
                Enter => self.mode = Mode::Edit,
                Char(' ') => file.included = !file.included,
                Esc if self.files.iter().any(|f| f.edited || !f.included) => {
                    self.mode = Mode::Discard
                }
                Esc => return Some(false),
                _ => {}
            }
        }
        None
    }

    fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let narrow = area.width < 75;
        let [header, warning, body, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(if area.width < 51 { 2 } else { 1 }),
            Constraint::Min(1),
            Constraint::Length(if narrow { 3 } else { 2 }),
        ])
        .areas(area);
        let count = self.files.iter().filter(|f| f.included).count();
        frame.render_widget(
            Paragraph::new(format!(
                " Report review   /   {count} of {} files included",
                self.files.len()
            ))
            .bold()
            .cyan(),
            header,
        );
        frame.render_widget(
            Paragraph::new(" WARNING: LOGS MAY CONTAIN SENSITIVE INFORMATION")
                .bold()
                .white()
                .on_red()
                .wrap(Wrap::default()),
            warning,
        );

        let [list, preview] = if narrow {
            if matches!(self.mode, Mode::Edit | Mode::Find) {
                [Rect::default(), body]
            } else {
                [body, Rect::default()]
            }
        } else {
            Layout::horizontal([Constraint::Length(30), Constraint::Min(1)]).areas(body)
        };
        let panel = |title: &str, active: bool| {
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(if active {
                    Style::new().cyan()
                } else {
                    Style::new().dark_gray()
                })
                .title(format!(" {title} "))
        };
        let items = self.files.iter().map(|f| {
            let mark = if f.included { "[x]" } else { "[ ]" };
            ListItem::new(format!("{mark} {}", f.name)).style(if f.included {
                Style::default()
            } else {
                Style::new().dark_gray()
            })
        });
        let files = List::new(items)
            .block(panel("Outgoing files", self.mode != Mode::Edit))
            .highlight_style(Style::new().black().on_cyan().bold());
        frame.render_stateful_widget(
            files,
            list,
            &mut ListState::default().with_selected(Some(self.selected)),
        );
        let f = &self.files[self.selected];
        let audience = if !f.included {
            "Excluded"
        } else if f.name == "summary.txt" {
            "Public or encrypted"
        } else {
            "Encrypted only"
        };
        let position = f.editor.cursor();
        let doc = panel(&f.name, self.mode == Mode::Edit)
            .title_bottom(Line::from(format!(" {audience} · Ln {} ", position.0 + 1)));
        let inner = doc.inner(preview);
        frame.render_widget(doc, preview);
        if !inner.is_empty() {
            frame.render_widget(&f.editor, inner);
        }
        let find;
        let hints = match self.mode {
            Mode::Find => {
                let status = if self.no_match {
                    "No match"
                } else {
                    "Enter Find"
                };
                find = format!("Find: {}\n{status}   Esc Back", self.query);
                &find
            }
            Mode::Files => {
                "↑↓ Select   Enter Edit   Space Include/exclude\nCtrl+S Apply locally   Esc Cancel"
            }
            Mode::Edit => "Ctrl+F Find   F3 Next   Ctrl+Z Undo\nEsc Files   Ctrl+S Apply locally",
            Mode::Discard => "Discard your edits?   D Discard   Enter or Esc Keep editing",
        };
        frame.render_widget(
            Paragraph::new(hints)
                .wrap(Wrap::default())
                .style(if self.mode == Mode::Discard {
                    Style::new().yellow().bold()
                } else {
                    Style::default()
                }),
            help,
        );
    }

    fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<bool> {
        loop {
            terminal.draw(|frame| self.draw(frame))?;
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(apply) = self.key(key) {
                        return Ok(apply);
                    }
                }
                Event::Paste(text) if self.mode == Mode::Edit => {
                    let file = &mut self.files[self.selected];
                    let plain = text.replace("\r\n", "\n").replace('\r', "\n");
                    if plain
                        .chars()
                        .all(|c| !c.is_control() || matches!(c, '\t' | '\n'))
                    {
                        file.edited |= file.editor.insert_str(plain);
                    }
                }
                _ => {}
            }
        }
    }
}

fn main() -> Result<ExitCode> {
    let files = env::args()
        .skip(1)
        .map(File::load)
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        !files.is_empty(),
        "Open Review/edit from the hardware test runner."
    );
    let mut review = Review {
        files,
        selected: 0,
        mode: Mode::Files,
        query: String::new(),
        no_match: false,
    };
    let mut terminal = ratatui::try_init()?;
    let result = execute!(io::stdout(), EnableBracketedPaste)
        .or_else(|err| {
            if err.kind() == io::ErrorKind::Unsupported {
                Ok(())
            } else {
                Err(err)
            }
        })
        .map_err(anyhow::Error::from)
        .and_then(|()| review.run(&mut terminal));
    let cleanup = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    cleanup?;
    if !result? {
        return Ok(ExitCode::from(2));
    }
    // These are disposable copies. The runner commits all of them atomically.
    for file in review.files {
        if !file.included {
            fs::remove_file(file.name)?;
        } else if file.edited {
            fs::write(file.name, file.editor.lines().join(file.newline))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn sample() -> Review {
        Review {
            files: vec![File {
                name: "summary.txt".into(),
                editor: TextArea::from(["Sample", "value.1"]),
                newline: "\n",
                included: true,
                edited: false,
            }],
            selected: 0,
            mode: Mode::Files,
            query: String::new(),
            no_match: false,
        }
    }

    #[test]
    fn find_literal_edit_undo_and_discard() {
        let mut review = sample();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let control = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);
        review.key(key(KeyCode::Enter));
        review.key(control('f'));
        review.key(key(KeyCode::Char('.')));
        review.key(key(KeyCode::Enter));
        assert_eq!(review.files[0].editor.cursor(), (1, 5));
        review.key(key(KeyCode::Char('x')));
        review.key(control('z'));
        assert_eq!(review.files[0].editor.lines()[1], "value.1");
        review.key(key(KeyCode::Esc));
        review.key(key(KeyCode::Char(' ')));
        assert!(!review.files[0].included);
        assert_eq!(review.key(key(KeyCode::Esc)), None);
        assert_eq!(review.key(key(KeyCode::Char('d'))), Some(false));
    }

    #[test]
    fn warning_and_apply_hint_survive_narrow_and_wide_layouts() {
        for width in [48, 100] {
            let mut term = Terminal::new(TestBackend::new(width, 15)).unwrap();
            let mut review = sample();
            for mode in [Mode::Files, Mode::Edit] {
                review.mode = mode;
                term.draw(|frame| review.draw(frame)).unwrap();
                let screen: String = term
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                assert!(screen.contains("WARNING: LOGS MAY CONTAIN SENSITIVE INFORMATION"));
                assert!(screen.contains("Apply locally"));
                assert!(screen.contains("summary.txt"));
            }
        }
    }
}
