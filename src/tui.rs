use std::{
    error::Error,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::Duration,
};

use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use unicode_width::UnicodeWidthChar;

use crate::{azure::AzureCli, matcher::ranked_indices, model::Subscription};

pub fn choose_subscription(
    azure: AzureCli,
    refresh: bool,
) -> Result<Option<Subscription>, Box<dyn Error>> {
    let receiver = load_in_background(azure, refresh);
    let mut terminal = ratatui::try_init()?;
    let result = run(&mut terminal, receiver);
    let restore_result = ratatui::try_restore();

    match (result, restore_result) {
        (Ok(selection), Ok(())) => Ok(selection),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(Box::new(error)),
    }
}

fn load_in_background(
    azure: AzureCli,
    refresh: bool,
) -> Receiver<Result<Vec<Subscription>, String>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = azure
            .list_subscriptions(refresh)
            .map_err(|error| error.to_string());
        let _ = sender.send(result);
    });
    receiver
}

fn run(
    terminal: &mut DefaultTerminal,
    receiver: Receiver<Result<Vec<Subscription>, String>>,
) -> Result<Option<Subscription>, Box<dyn Error>> {
    let mut app = App::new();

    loop {
        app.receive_accounts(&receiver);
        terminal.draw(|frame| render(frame, &mut app))?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(action) = app.handle_key(key) {
                        return Ok(match action {
                            Action::Cancel => None,
                            Action::Select => app.selected().cloned(),
                        });
                    }
                }
                Event::Paste(text) => app.append_query(&text),
                _ => {}
            }
        }
    }
}

#[derive(Debug)]
struct App {
    subscriptions: Vec<Subscription>,
    filtered: Vec<usize>,
    query: String,
    list_state: ListState,
    loading: bool,
    load_error: Option<String>,
}

impl App {
    fn new() -> Self {
        Self {
            subscriptions: Vec::new(),
            filtered: Vec::new(),
            query: String::new(),
            list_state: ListState::default(),
            loading: true,
            load_error: None,
        }
    }

    fn receive_accounts(&mut self, receiver: &Receiver<Result<Vec<Subscription>, String>>) {
        if !self.loading {
            return;
        }

        match receiver.try_recv() {
            Ok(Ok(subscriptions)) => {
                self.loading = false;
                self.subscriptions = subscriptions;
                self.refresh_filter();
            }
            Ok(Err(error)) => {
                self.loading = false;
                self.load_error = Some(error);
                self.refresh_filter();
            }
            Err(TryRecvError::Disconnected) => {
                self.loading = false;
                self.load_error = Some("subscription loader stopped unexpectedly".to_owned());
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Char('j') => {
                    self.next();
                    return None;
                }
                KeyCode::Char('k') => {
                    self.previous();
                    return None;
                }
                _ => {}
            }
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') => Some(Action::Cancel),
                KeyCode::Char('u') => {
                    self.query.clear();
                    self.refresh_filter();
                    None
                }
                KeyCode::Char('n') => {
                    self.next();
                    None
                }
                KeyCode::Char('p') => {
                    self.previous();
                    None
                }
                _ => None,
            };
        }

        match key.code {
            KeyCode::Esc => Some(Action::Cancel),
            KeyCode::Enter if self.selected().is_some() => Some(Action::Select),
            KeyCode::Up => {
                self.previous();
                None
            }
            KeyCode::Down => {
                self.next();
                None
            }
            KeyCode::Home => {
                self.select_first();
                None
            }
            KeyCode::End => {
                self.select_last();
                None
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.refresh_filter();
                None
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.query.push(character);
                self.refresh_filter();
                None
            }
            _ => None,
        }
    }

    fn append_query(&mut self, text: &str) {
        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        self.refresh_filter();
    }

    fn refresh_filter(&mut self) {
        self.filtered = ranked_indices(&self.query, &self.subscriptions);
        self.list_state
            .select((!self.filtered.is_empty()).then_some(0));
    }

    fn selected(&self) -> Option<&Subscription> {
        self.list_state
            .selected()
            .and_then(|selected| self.filtered.get(selected))
            .and_then(|&index| self.subscriptions.get(index))
    }

    fn next(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let next = self
            .list_state
            .selected()
            .map_or(0, |selected| (selected + 1) % self.filtered.len());
        self.list_state.select(Some(next));
    }

    fn previous(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let previous = self.list_state.selected().map_or(0, |selected| {
            if selected == 0 {
                self.filtered.len() - 1
            } else {
                selected - 1
            }
        });
        self.list_state.select(Some(previous));
    }

    fn select_first(&mut self) {
        self.list_state
            .select((!self.filtered.is_empty()).then_some(0));
    }

    fn select_last(&mut self) {
        self.list_state
            .select((!self.filtered.is_empty()).then_some(self.filtered.len() - 1));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Cancel,
    Select,
}

fn render(frame: &mut Frame, app: &mut App) {
    let [search_area, list_area, footer_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_search(frame, app, search_area);
    render_list(frame, app, list_area);
    render_footer(frame, app, footer_area);
}

fn render_search(frame: &mut Frame, app: &App, area: Rect) {
    let max_width = area.width.saturating_sub(2) as usize;
    let visible_query = trailing_width(&app.query, max_width);
    let query_width = visible_query
        .chars()
        .map(|character| character.width().unwrap_or(0))
        .sum::<usize>();
    let search = Paragraph::new(visible_query.as_str()).block(
        Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(Color::Cyan))
            .title(" Search subscriptions "),
    );
    frame.render_widget(search, area);

    if area.width > 2 {
        frame.set_cursor_position(Position::new(
            area.x + 1 + query_width.min(max_width) as u16,
            area.y + 1,
        ));
    }
}

fn render_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let items = app
        .filtered
        .iter()
        .filter_map(|&index| app.subscriptions.get(index))
        .map(|subscription| {
            let marker = if subscription.is_default {
                "● "
            } else {
                "  "
            };
            let marker_style = if subscription.is_default {
                Style::new().fg(Color::Green)
            } else {
                Style::new()
            };
            let name_style = if subscription.is_default {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };

            ListItem::new(Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(subscription.name.clone(), name_style),
                Span::styled(
                    format!("  {}", subscription.id),
                    Style::new().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect::<Vec<_>>();

    let title = if app.loading {
        " Subscriptions · loading Azure CLI… ".to_owned()
    } else if let Some(error) = &app.load_error {
        format!(" Azure CLI error · {error} ")
    } else {
        format!(
            " Subscriptions · {} of {} ",
            app.filtered.len(),
            app.subscriptions.len()
        )
    };

    let list = List::new(items)
        .block(
            Block::new()
                .borders(Borders::LEFT | Borders::RIGHT)
                .border_style(Style::new().fg(Color::DarkGray))
                .title(title),
        )
        .highlight_symbol("› ")
        .highlight_style(
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut app.list_state);

    if !app.loading && app.load_error.is_none() && app.filtered.is_empty() {
        let message = if app.subscriptions.is_empty() {
            "No enabled subscriptions found"
        } else {
            "No matching subscriptions"
        };
        frame.render_widget(
            Paragraph::new(message)
                .centered()
                .style(Style::new().fg(Color::Yellow)),
            area,
        );
    }
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let status = if let Some(selected) = app.selected() {
        format!(
            " Enter switch · ↑/↓ or Alt+j/k navigate · Esc cancel    tenant {} ",
            selected.tenant_id
        )
    } else {
        " Type to search · ↑/↓ or Alt+j/k navigate · Esc cancel ".to_owned()
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::new().fg(Color::DarkGray)),
        area,
    );
}

fn trailing_width(text: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut reversed = Vec::new();

    for character in text.chars().rev() {
        let character_width = character.width().unwrap_or(0);
        if width + character_width > max_width {
            break;
        }
        width += character_width;
        reversed.push(character);
    }

    reversed.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::{Action, App, trailing_width};
    use crate::model::Subscription;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn subscriptions() -> Vec<Subscription> {
        ["Finance", "HR Production", "HR Development"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| Subscription {
                id: index.to_string(),
                name: name.to_owned(),
                tenant_id: "tenant".to_owned(),
                is_default: index == 0,
                state: "Enabled".to_owned(),
            })
            .collect()
    }

    fn loaded_app() -> App {
        let mut app = App::new();
        app.loading = false;
        app.subscriptions = subscriptions();
        app.refresh_filter();
        app
    }

    #[test]
    fn typed_characters_filter_without_an_input_mode() {
        let mut app = loaded_app();

        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));

        assert_eq!(app.query, "hr");
        assert_eq!(app.filtered.len(), 2);
        assert_eq!(app.selected().unwrap().name, "HR Development");
    }

    #[test]
    fn enter_selects_and_escape_cancels() {
        let mut app = loaded_app();

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Select)
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Action::Cancel)
        );
    }

    #[test]
    fn navigation_wraps_the_filtered_list() {
        let mut app = loaded_app();
        app.query = "hr".to_owned();
        app.refresh_filter();

        app.previous();
        assert_eq!(app.selected().unwrap().name, "HR Production");
        app.next();
        assert_eq!(app.selected().unwrap().name, "HR Development");
    }

    #[test]
    fn alt_j_and_k_navigate_without_becoming_search_text() {
        let mut app = loaded_app();

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT));
        assert_eq!(app.selected().unwrap().name, "HR Development");
        assert_eq!(app.query, "");

        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT));
        assert_eq!(app.selected().unwrap().name, "Finance");
        assert_eq!(app.query, "");

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.query, "j");
    }

    #[test]
    fn keeps_the_visible_end_of_a_long_query() {
        assert_eq!(trailing_width("123456", 4), "3456");
        assert_eq!(trailing_width("a界b", 3), "界b");
    }
}
