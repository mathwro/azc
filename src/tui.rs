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

use crate::{
    azure::AzureCli,
    matcher::{ranked_indices, ranked_tenant_indices},
    model::{Subscription, Tenant, TenantDescription, tenants_from_subscriptions},
};

type LoadedAccounts = Result<(Vec<Subscription>, Vec<TenantDescription>), String>;
pub fn choose_subscription(
    azure: AzureCli,
    refresh: bool,
) -> Result<Option<Subscription>, Box<dyn Error>> {
    choose(azure, refresh, false)
}

pub fn choose_tenant(
    azure: AzureCli,
    refresh: bool,
) -> Result<Option<Subscription>, Box<dyn Error>> {
    choose(azure, refresh, true)
}

fn choose(
    azure: AzureCli,
    refresh: bool,
    tenant_mode: bool,
) -> Result<Option<Subscription>, Box<dyn Error>> {
    let receiver = load_in_background(azure, refresh);
    let mut terminal = ratatui::try_init()?;
    let result = run(&mut terminal, receiver, tenant_mode);
    let restore_result = ratatui::try_restore();

    match (result, restore_result) {
        (Ok(selection), Ok(())) => Ok(selection),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(Box::new(error)),
    }
}

fn load_in_background(azure: AzureCli, refresh: bool) -> Receiver<LoadedAccounts> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = azure
            .list_subscriptions(refresh)
            .map(|subscriptions| {
                let descriptions = azure.list_tenants().unwrap_or_default();
                (subscriptions, descriptions)
            })
            .map_err(|error| error.to_string());
        let _ = sender.send(result);
    });
    receiver
}

fn run(
    terminal: &mut DefaultTerminal,
    receiver: Receiver<LoadedAccounts>,
    tenant_mode: bool,
) -> Result<Option<Subscription>, Box<dyn Error>> {
    let mut app = App::new(tenant_mode);

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
    tenants: Vec<Tenant>,
    filtered: Vec<usize>,
    query: String,
    list_state: ListState,
    loading: bool,
    load_error: Option<String>,
    tenant_mode: bool,
}

impl App {
    fn new(tenant_mode: bool) -> Self {
        Self {
            subscriptions: Vec::new(),
            tenants: Vec::new(),
            filtered: Vec::new(),
            query: String::new(),
            list_state: ListState::default(),
            loading: true,
            load_error: None,
            tenant_mode,
        }
    }

    fn receive_accounts(&mut self, receiver: &Receiver<LoadedAccounts>) {
        if !self.loading {
            return;
        }

        match receiver.try_recv() {
            Ok(Ok((subscriptions, descriptions))) => {
                self.loading = false;
                self.tenants = tenants_from_subscriptions(&subscriptions, &descriptions);
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
                KeyCode::Char('t') => {
                    self.tenant_mode = !self.tenant_mode;
                    self.query.clear();
                    self.refresh_filter();
                    None
                }
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
        self.filtered = if self.tenant_mode {
            ranked_tenant_indices(&self.query, &self.tenants)
        } else {
            ranked_indices(&self.query, &self.subscriptions)
        };
        self.list_state
            .select((!self.filtered.is_empty()).then_some(0));
    }

    fn selected(&self) -> Option<&Subscription> {
        let subscription_index = self
            .list_state
            .selected()
            .and_then(|selected| self.filtered.get(selected))
            .map(|&index| {
                if self.tenant_mode {
                    self.tenants[index].subscription_index
                } else {
                    index
                }
            })?;
        self.subscriptions.get(subscription_index)
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
    let title = if app.tenant_mode {
        " Search tenants "
    } else {
        " Search subscriptions "
    };
    let search = Paragraph::new(visible_query.as_str()).block(
        Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(Color::Cyan))
            .title(title),
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
    let items = if app.tenant_mode {
        app.filtered
            .iter()
            .filter_map(|&index| app.tenants.get(index))
            .map(|tenant| {
                let marker = if tenant.is_current { "● " } else { "  " };
                let marker_style = if tenant.is_current {
                    Style::new().fg(Color::Green)
                } else {
                    Style::new()
                };
                let id_style = if tenant.is_current {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };

                let tenant_name = if tenant.name.is_empty() {
                    tenant.id.as_str()
                } else {
                    tenant.name.as_str()
                };
                let tenant_details = if tenant_name.eq_ignore_ascii_case(&tenant.id) {
                    format!("  {} subscription(s)", tenant.subscription_count)
                } else {
                    format!(
                        "  {} · {} subscription(s)",
                        tenant.id, tenant.subscription_count
                    )
                };

                ListItem::new(Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(tenant_name.to_owned(), id_style),
                    Span::styled(tenant_details, Style::new().fg(Color::DarkGray)),
                ]))
            })
            .collect::<Vec<_>>()
    } else {
        app.filtered
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
            .collect::<Vec<_>>()
    };

    let current_tenant = app
        .tenants
        .iter()
        .find(|tenant| tenant.is_current)
        .map_or_else(
            || "unknown".to_owned(),
            |tenant| {
                if tenant.name.is_empty() {
                    tenant.id.clone()
                } else {
                    format!("{} ({})", tenant.name, tenant.id)
                }
            },
        );
    let title = if app.loading {
        format!(
            " {} · loading Azure CLI… ",
            if app.tenant_mode {
                "Tenants"
            } else {
                "Subscriptions"
            }
        )
    } else if let Some(error) = &app.load_error {
        format!(" Azure CLI error · {error} ")
    } else {
        format!(
            " {} · {} of {} · current tenant {} ",
            if app.tenant_mode {
                "Tenants"
            } else {
                "Subscriptions"
            },
            app.filtered.len(),
            if app.tenant_mode {
                app.tenants.len()
            } else {
                app.subscriptions.len()
            },
            current_tenant
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
        let message = if app.tenant_mode {
            "No tenants with enabled subscriptions found"
        } else if app.subscriptions.is_empty() {
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
    let current_tenant = app
        .tenants
        .iter()
        .find(|tenant| tenant.is_current)
        .map_or_else(
            || "unknown".to_owned(),
            |tenant| {
                if tenant.name.is_empty() {
                    tenant.id.clone()
                } else {
                    format!("{} ({})", tenant.name, tenant.id)
                }
            },
        );
    let status = if app.selected().is_some() {
        format!(
            " Enter switch · Ctrl+T {} · Esc cancel · current tenant {} ",
            if app.tenant_mode {
                "subscriptions"
            } else {
                "tenants"
            },
            current_tenant
        )
    } else {
        format!(
            " Type to search · Ctrl+T {} · Esc cancel · current tenant {} ",
            if app.tenant_mode {
                "subscriptions"
            } else {
                "tenants"
            },
            current_tenant
        )
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
    use super::{Action, App, render, trailing_width};
    use crate::{
        azure::AzureCli,
        model::{Subscription, TenantDescription, tenants_from_subscriptions},
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
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
        let mut app = App::new(false);
        app.loading = false;
        app.subscriptions = subscriptions();
        app.tenants = tenants_from_subscriptions(&app.subscriptions, &[]);
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
    fn ctrl_t_switches_to_tenants_and_selects_their_current_subscription() {
        let mut app = loaded_app();
        app.subscriptions[1].tenant_id = "other-tenant".to_owned();
        app.subscriptions[1].is_default = false;
        app.subscriptions[2].tenant_id = "other-tenant".to_owned();
        app.tenants = tenants_from_subscriptions(&app.subscriptions, &[]);

        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));

        assert!(app.tenant_mode);
        assert_eq!(app.filtered.len(), 2);
        assert_eq!(app.selected().unwrap().tenant_id, "tenant");
        app.next();
        assert_eq!(app.selected().unwrap().tenant_id, "other-tenant");
    }

    #[test]
    fn renders_display_names_for_every_tenant_from_json() {
        let subscriptions = vec![
            Subscription {
                id: "subscription-a".to_owned(),
                name: "Alpha".to_owned(),
                tenant_id: "tenant-a".to_owned(),
                is_default: true,
                state: "Enabled".to_owned(),
            },
            Subscription {
                id: "subscription-b".to_owned(),
                name: "Beta".to_owned(),
                tenant_id: "tenant-b".to_owned(),
                is_default: false,
                state: "Enabled".to_owned(),
            },
        ];
        let mut descriptions: Vec<TenantDescription> =
            serde_json::from_str(r#"[{"tenantId":"tenant-a","displayName":"Tenant A"}]"#).unwrap();
        let metadata: Vec<TenantDescription> =
            serde_json::from_str(r#"[{"tenantId":"tenant-b","displayName":"Tenant B"}]"#).unwrap();
        AzureCli::merge_tenant_metadata(&mut descriptions, &metadata);
        let mut app = App::new(true);
        app.loading = false;
        app.subscriptions = subscriptions;
        app.tenants = tenants_from_subscriptions(&app.subscriptions, &descriptions);
        app.refresh_filter();

        let backend = TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let frame = terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = frame
            .buffer
            .content()
            .iter()
            .fold(String::new(), |mut text, cell| {
                text.push_str(cell.symbol());
                text
            });

        assert!(rendered.contains("Tenant A"));
        assert!(rendered.contains("Tenant B"));
        assert!(rendered.contains("tenant-a"));
        assert!(rendered.contains("tenant-b"));
    }

    #[test]
    fn keeps_the_visible_end_of_a_long_query() {
        assert_eq!(trailing_width("123456", 4), "3456");
        assert_eq!(trailing_width("a界b", 3), "界b");
    }
}
