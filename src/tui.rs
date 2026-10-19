use std::{
    error::Error,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, SyncSender, TryRecvError},
    },
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
    cache::Cache,
    matcher::{ranked_indices, ranked_tenant_indices},
    model::{Subscription, Tenant, TenantDescription, tenants_from_subscriptions},
};

enum LoadedAccounts {
    RefreshStarted,
    Cached(Vec<Subscription>, Vec<TenantDescription>),
    Subscriptions(Arc<Vec<Subscription>>),
    Tenants(Vec<TenantDescription>),
    Failed(String),
}

struct AccountLoader {
    accounts: Receiver<LoadedAccounts>,
    refresh: SyncSender<()>,
}
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
    let loader = load_in_background(azure, refresh);
    let mut terminal = ratatui::try_init()?;
    let result = run(&mut terminal, loader, tenant_mode, refresh);
    let restore_result = ratatui::try_restore();

    match (result, restore_result) {
        (Ok(selection), Ok(())) => Ok(selection),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(Box::new(error)),
    }
}

fn load_in_background(azure: AzureCli, refresh: bool) -> AccountLoader {
    let (sender, accounts) = mpsc::channel();
    let (request, requests) = mpsc::sync_channel(1);
    thread::spawn(move || {
        if !load_accounts(&azure, refresh, &sender) {
            return;
        }
        for () in requests {
            if !load_accounts(&azure, true, &sender) {
                break;
            }
        }
    });
    AccountLoader {
        accounts,
        refresh: request,
    }
}

fn load_accounts(azure: &AzureCli, refresh: bool, sender: &Sender<LoadedAccounts>) -> bool {
    if refresh && sender.send(LoadedAccounts::RefreshStarted).is_err() {
        return false;
    }
    let cache = Cache::from_env();
    if !refresh
        && let Some(snapshot) = cache.as_ref().and_then(Cache::load)
        && sender
            .send(LoadedAccounts::Cached(
                snapshot.subscriptions,
                snapshot.descriptions,
            ))
            .is_err()
    {
        return false;
    }
    let subscriptions: Arc<Vec<Subscription>> = match azure.list_subscriptions(refresh) {
        Ok(subscriptions) => subscriptions.into(),
        Err(error) => {
            return sender
                .send(LoadedAccounts::Failed(error.to_string()))
                .is_ok();
        }
    };
    // Save and publish subscriptions before waiting for tenant metadata.
    if let Some(cache) = &cache {
        let _ = cache.save(&subscriptions, &[]);
    }
    if sender
        .send(LoadedAccounts::Subscriptions(Arc::clone(&subscriptions)))
        .is_err()
    {
        return false;
    }
    let descriptions = azure.list_tenants().unwrap_or_default();
    if let Some(cache) = &cache {
        let _ = cache.save(&subscriptions, &descriptions);
    }
    sender.send(LoadedAccounts::Tenants(descriptions)).is_ok()
}

fn run(
    terminal: &mut DefaultTerminal,
    loader: AccountLoader,
    tenant_mode: bool,
    refresh: bool,
) -> Result<Option<Subscription>, Box<dyn Error>> {
    let mut app = App::new(tenant_mode);
    app.refreshing = refresh;

    loop {
        app.receive_accounts(&loader.accounts);
        terminal.draw(|frame| render(frame, &mut app))?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(action) = app.handle_key(key) {
                        match action {
                            Action::Cancel => return Ok(None),
                            Action::Select => return Ok(app.selected().cloned()),
                            Action::Refresh => {
                                if loader.refresh.try_send(()).is_err() {
                                    app.loading = false;
                                    app.refreshing = false;
                                    app.awaiting_refresh = false;
                                    app.load_error =
                                        Some("subscription loader stopped unexpectedly".to_owned());
                                }
                            }
                        }
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
    subscriptions: Arc<Vec<Subscription>>,
    descriptions: Vec<TenantDescription>,
    has_accounts: bool,
    cached: bool,
    tenants: Vec<Tenant>,
    filtered: Vec<usize>,
    query: String,
    list_state: ListState,
    loading: bool,
    refreshing: bool,
    awaiting_refresh: bool,
    load_error: Option<String>,
    tenant_mode: bool,
    tenant_scope: Option<String>,
}

impl App {
    fn new(tenant_mode: bool) -> Self {
        Self {
            subscriptions: Arc::new(Vec::new()),
            descriptions: Vec::new(),
            has_accounts: false,
            cached: false,
            tenants: Vec::new(),
            filtered: Vec::new(),
            query: String::new(),
            list_state: ListState::default(),
            loading: true,
            refreshing: false,
            awaiting_refresh: false,
            load_error: None,
            tenant_mode,
            tenant_scope: None,
        }
    }

    fn receive_accounts(&mut self, receiver: &Receiver<LoadedAccounts>) {
        if !self.loading {
            return;
        }

        loop {
            match receiver.try_recv() {
                Ok(LoadedAccounts::RefreshStarted) => {
                    self.awaiting_refresh = false;
                    self.refreshing = true;
                }
                // The initial load may finish after Ctrl+R queues a server refresh.
                Ok(_) if self.awaiting_refresh => {}
                Ok(LoadedAccounts::Cached(subscriptions, descriptions)) => {
                    self.descriptions = descriptions;
                    self.replace_subscriptions(subscriptions.into());
                    self.cached = true;
                }
                Ok(LoadedAccounts::Subscriptions(subscriptions)) => {
                    self.replace_subscriptions(subscriptions);
                    self.cached = false;
                }
                Ok(LoadedAccounts::Tenants(descriptions)) => {
                    self.descriptions = descriptions;
                    self.replace_subscriptions(Arc::clone(&self.subscriptions));
                    self.loading = false;
                    self.refreshing = false;
                    break;
                }
                Ok(LoadedAccounts::Failed(error)) => {
                    self.loading = false;
                    self.refreshing = false;
                    self.load_error = Some(error);
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    self.loading = false;
                    self.refreshing = false;
                    self.awaiting_refresh = false;
                    self.load_error = Some("subscription loader stopped unexpectedly".to_owned());
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
    }

    fn replace_subscriptions(&mut self, subscriptions: Arc<Vec<Subscription>>) {
        let selected_index = self.selected_index();
        let previous = std::mem::replace(&mut self.subscriptions, subscriptions);
        let selected_id = selected_index
            .and_then(|index| previous.get(index))
            .map(|subscription| subscription.id.as_str());
        let previous_tenants = std::mem::replace(
            &mut self.tenants,
            tenants_from_subscriptions(&self.subscriptions, &self.descriptions),
        );
        let selected_tenant_id = self
            .list_state
            .selected()
            .and_then(|selected| self.filtered.get(selected))
            .and_then(|&index| previous_tenants.get(index))
            .map(|tenant| tenant.id.as_str());
        self.refresh_filter();
        if self.has_accounts {
            let selected = if self.tenant_mode {
                selected_tenant_id.and_then(|id| {
                    self.filtered.iter().position(|&index| {
                        self.tenants[index]
                            .id
                            .trim()
                            .eq_ignore_ascii_case(id.trim())
                    })
                })
            } else {
                selected_id.and_then(|id| {
                    self.filtered
                        .iter()
                        .position(|&index| self.subscriptions[index].id == id)
                })
            };
            self.list_state.select(selected);
        }
        self.has_accounts = true;
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
                KeyCode::Char('r') if !self.refreshing => {
                    self.loading = true;
                    self.refreshing = true;
                    self.awaiting_refresh = true;
                    self.load_error = None;
                    Some(Action::Refresh)
                }
                KeyCode::Char('t') => {
                    self.tenant_mode = !self.tenant_mode;
                    self.tenant_scope = None;
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
            KeyCode::Enter if self.selected().is_some() => {
                if self.tenant_mode {
                    self.tenant_scope = Some(self.selected()?.tenant_id.clone());
                    self.tenant_mode = false;
                    self.query.clear();
                    self.refresh_filter();
                    None
                } else {
                    Some(Action::Select)
                }
            }
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
            let mut indices = ranked_indices(&self.query, &self.subscriptions);
            if let Some(tenant_id) = &self.tenant_scope {
                indices.retain(|&index| {
                    self.subscriptions[index]
                        .tenant_id
                        .trim()
                        .eq_ignore_ascii_case(tenant_id.trim())
                });
            }
            indices
        };
        self.list_state
            .select((!self.filtered.is_empty()).then_some(0));
    }

    fn selected(&self) -> Option<&Subscription> {
        self.subscriptions.get(self.selected_index()?)
    }

    fn selected_index(&self) -> Option<usize> {
        self.list_state
            .selected()
            .and_then(|selected| self.filtered.get(selected))
            .map(|&index| {
                if self.tenant_mode {
                    self.tenants[index].subscription_index
                } else {
                    index
                }
            })
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
    Refresh,
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

                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(marker, marker_style),
                        Span::styled(tenant.label(), id_style),
                    ]),
                    Line::styled(
                        format!(
                            "  {} · {} subscription(s)",
                            tenant.id, tenant.subscription_count
                        ),
                        Style::new().fg(Color::DarkGray),
                    ),
                ])
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
    let list_name = if app.tenant_mode {
        "Tenants".to_owned()
    } else if let Some(id) = &app.tenant_scope {
        let label = app
            .tenants
            .iter()
            .find(|tenant| tenant.id.trim().eq_ignore_ascii_case(id.trim()))
            .map_or_else(|| id.clone(), Tenant::label);
        format!("Subscriptions in {label}")
    } else {
        "Subscriptions".to_owned()
    };
    let title = if app.loading {
        format!(
            " {} · {} ",
            list_name,
            if app.refreshing && app.cached {
                "cached · refreshing Azure…"
            } else if app.refreshing {
                "refreshing Azure…"
            } else if app.cached {
                "cached · updating…"
            } else if app.has_accounts {
                "loading tenant names…"
            } else {
                "loading Azure CLI…"
            }
        )
    } else if let Some(error) = &app.load_error {
        format!(
            " {}Azure CLI error · {error} ",
            if app.cached { "cached · " } else { "" }
        )
    } else {
        format!(
            " {} · {} of {} · current tenant {} ",
            list_name,
            app.filtered.len(),
            if app.tenant_mode {
                app.tenants.len()
            } else {
                app.subscriptions
                    .iter()
                    .filter(|subscription| {
                        app.tenant_scope.as_ref().is_none_or(|id| {
                            subscription
                                .tenant_id
                                .trim()
                                .eq_ignore_ascii_case(id.trim())
                        })
                    })
                    .count()
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
            " Enter {} · Ctrl+R refresh · Ctrl+T {} · Esc cancel · current tenant {} ",
            if app.tenant_mode {
                "subscriptions"
            } else {
                "switch"
            },
            if app.tenant_mode {
                "subscriptions"
            } else {
                "tenants"
            },
            current_tenant
        )
    } else {
        format!(
            " Type to search · Ctrl+R refresh · Ctrl+T {} · Esc cancel · current tenant {} ",
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
    use super::{Action, App, LoadedAccounts, render, trailing_width};
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
        app.replace_subscriptions(subscriptions().into());
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
    fn ctrl_r_starts_loading_without_changing_search_or_switch_target() {
        let mut app = loaded_app();
        app.append_query("hr");
        app.next();
        app.load_error = Some("previous listing failed".to_owned());

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));

        assert!(app.loading);
        assert!(app.load_error.is_none());
        assert_eq!(app.query, "hr");
        assert_eq!(app.selected().unwrap().id, "1");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn requested_refresh_ignores_initial_loader_results() {
        let mut app = App::new(false);
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(LoadedAccounts::Cached(subscriptions(), vec![]))
            .unwrap();
        app.receive_accounts(&receiver);
        app.append_query("hr");
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        sender
            .send(LoadedAccounts::Failed("obsolete load failed".to_owned()))
            .unwrap();
        sender
            .send(LoadedAccounts::Subscriptions(vec![].into()))
            .unwrap();
        sender.send(LoadedAccounts::Tenants(vec![])).unwrap();

        app.receive_accounts(&receiver);

        assert!(app.loading);
        assert!(app.load_error.is_none());
        assert_eq!(app.query, "hr");
        assert_eq!(app.selected().unwrap().id, "2");

        sender.send(LoadedAccounts::RefreshStarted).unwrap();
        sender
            .send(LoadedAccounts::Subscriptions(subscriptions().into()))
            .unwrap();
        sender.send(LoadedAccounts::Tenants(vec![])).unwrap();
        app.receive_accounts(&receiver);
        assert!(!app.loading);
        assert!(app.load_error.is_none());
        assert_eq!(app.query, "hr");
        assert_eq!(app.selected().unwrap().id, "2");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(Action::Refresh)
        );
    }

    #[test]
    fn failed_refresh_retains_results_and_can_be_requested_again() {
        let mut app = loaded_app();
        app.append_query("hr");
        app.next();
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(LoadedAccounts::RefreshStarted).unwrap();
        sender
            .send(LoadedAccounts::Failed("refresh failed".to_owned()))
            .unwrap();

        app.receive_accounts(&receiver);

        assert!(!app.loading);
        assert!(app.load_error.is_some());
        assert_eq!(app.query, "hr");
        assert_eq!(app.selected().unwrap().id, "1");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(Action::Refresh)
        );
        assert!(app.loading);
        assert!(app.load_error.is_none());
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
        let subscriptions = std::sync::Arc::make_mut(&mut app.subscriptions);
        subscriptions[1].tenant_id = "other-tenant".to_owned();
        subscriptions[1].is_default = false;
        subscriptions[2].tenant_id = "other-tenant".to_owned();
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
        let subscriptions: Vec<Subscription> = serde_json::from_str(
            r#"[{"id":"subscription-a","name":"Alpha","tenantId":"tenant-a","isDefault":true},
                {"id":"subscription-b","name":"Beta","tenantId":"tenant-b"}]"#,
        )
        .unwrap();
        let mut descriptions: Vec<TenantDescription> = serde_json::from_str(
            r#"[{"tenantId":"tenant-a","displayName":"Tenant A"},{"tenantId":"tenant-b"}]"#,
        )
        .unwrap();
        let metadata: Vec<TenantDescription> =
            serde_json::from_str(r#"[{"tenantId":"tenant-b","displayName":"Tenant B"},{"tenantId":"tenant-b","defaultDomain":"b.example"}]"#).unwrap();
        AzureCli::merge_tenant_metadata(&mut descriptions, &metadata);
        let mut app = App::new(true);
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(LoadedAccounts::Cached(subscriptions, vec![]))
            .unwrap();
        app.receive_accounts(&receiver);
        app.next();
        assert_eq!(app.selected().unwrap().id, "subscription-b");
        sender.send(LoadedAccounts::Tenants(descriptions)).unwrap();
        app.receive_accounts(&receiver);
        assert_eq!(app.selected().unwrap().id, "subscription-b");

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
    fn tenant_labels_use_names_domains_and_secondary_ids_from_json() {
        let subscriptions: Vec<Subscription> = serde_json::from_str(
            r#"[{"id":"a","name":"Alpha","tenantId":"tenant-a"},
                {"id":"b","name":"Beta","tenantId":"tenant-b"},
                {"id":"c","name":"Gamma","tenantId":"tenant-c"},
                {"id":"d","name":"Delta","tenantId":"tenant-d"}]"#,
        )
        .unwrap();
        let mut descriptions = serde_json::from_str(
            r#"[{"tenantId":"tenant-a","displayName":"  GN Store Nord  ","defaultDomain":null},
                {"tenantId":"tenant-b","displayName":"11111111-2222-3333-4444-555555555555"},
                {"tenantId":"tenant-c","displayName":"  "},
                {"tenantId":"tenant-c","displayName":"Tenant C"}]"#,
        )
        .unwrap();
        let metadata: Vec<TenantDescription> = serde_json::from_str(
            r#"[{"tenantId":"tenant-a","displayName":null,"defaultDomain":null},
                {"tenantId":" TENANT-A ","defaultDomain":" gn.group "},
                {"tenantId":"tenant-b","defaultDomain":"jabra4.onmicrosoft.com"}]"#,
        )
        .unwrap();
        AzureCli::merge_tenant_metadata(&mut descriptions, &metadata);
        let mut app = App::new(true);
        app.descriptions = descriptions;
        app.replace_subscriptions(subscriptions.into());
        app.loading = false;
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        let frame = terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let lines: Vec<String> = frame
            .buffer
            .content()
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        for (label, id) in [
            ("GN Store Nord (gn.group)", "tenant-a"),
            ("jabra4.onmicrosoft.com", "tenant-b"),
            ("Tenant C", "tenant-c"),
        ] {
            let row = lines.iter().position(|line| line.contains(label)).unwrap();
            assert!(!lines[row].contains(id));
            assert!(lines[row + 1].contains(id));
        }
        assert!(lines.iter().any(|line| line.contains("tenant-d")));
        assert!(!lines.iter().any(|line| line.contains("11111111-2222")));
        app.append_query("gn.group");
        assert_eq!(app.selected().unwrap().id, "a");
    }

    #[test]
    fn tenant_enter_opens_only_its_subscriptions_before_selecting() {
        let mut app = loaded_app();
        let mut subscriptions = subscriptions();
        subscriptions[1].tenant_id = " Other-Tenant ".to_owned();
        subscriptions[2].tenant_id = "other-tenant".to_owned();
        app.replace_subscriptions(subscriptions.into());
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        app.append_query("other");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert!(!app.tenant_mode);
        assert!(app.query.is_empty());
        assert_eq!(app.filtered.len(), 2);
        assert_eq!(app.selected().unwrap().id, "2");
        app.next();
        assert_eq!(app.selected().unwrap().id, "1");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Select)
        );
        app.append_query("Finance");
        assert!(app.selected().is_none());
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(app.tenant_mode);
        assert_eq!(app.filtered.len(), 2);
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.filtered.len(), 3);
    }

    #[test]
    fn scoped_subscription_updates_never_select_a_different_tenant() {
        let mut app = loaded_app();
        let mut initial = subscriptions();
        initial[1].tenant_id = "other".to_owned();
        initial[2].tenant_id = "other".to_owned();
        app.replace_subscriptions(initial.clone().into());
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        app.append_query("other");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        app.append_query("hr");
        app.next();
        initial.reverse();
        app.replace_subscriptions(initial.clone().into());
        assert_eq!(app.query, "hr");
        assert_eq!(app.selected().unwrap().id, "1");
        initial.retain(|subscription| subscription.tenant_id != "other");
        app.replace_subscriptions(initial.into());
        assert!(app.selected().is_none());
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(app.filtered.is_empty());
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Action::Cancel)
        );
    }

    #[test]
    fn background_updates_preserve_query_and_selected_id() {
        let mut app = App::new(false);
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(LoadedAccounts::Cached(subscriptions(), vec![]))
            .unwrap();
        app.receive_accounts(&receiver);
        app.append_query("hr");
        app.next();
        assert_eq!(app.selected().unwrap().id, "1");
        let mut updated = subscriptions();
        updated[1].name = "HR Zebra".to_owned();
        updated.reverse();
        sender
            .send(LoadedAccounts::Subscriptions(updated.into()))
            .unwrap();

        app.receive_accounts(&receiver);

        assert_eq!(app.query, "hr");
        assert_eq!(app.selected().unwrap().id, "1");
    }

    #[test]
    fn removed_selection_requires_explicit_navigation_before_enter() {
        let mut app = loaded_app();
        app.loading = true;
        let (sender, receiver) = std::sync::mpsc::channel();
        let updated: Vec<_> = subscriptions().into_iter().skip(1).collect();
        sender
            .send(LoadedAccounts::Subscriptions(updated.into()))
            .unwrap();

        app.receive_accounts(&receiver);

        assert!(app.selected().is_none());
        sender.send(LoadedAccounts::Tenants(vec![])).unwrap();
        app.receive_accounts(&receiver);
        assert!(app.selected().is_none());
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        app.next();
        assert_eq!(app.selected().unwrap().id, "2");
    }

    #[test]
    fn fresh_subscriptions_are_searchable_before_tenant_metadata_arrives() {
        let mut app = App::new(false);
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(LoadedAccounts::Subscriptions(subscriptions().into()))
            .unwrap();
        app.receive_accounts(&receiver);
        app.append_query("hr");
        assert!(app.loading);
        assert_eq!(app.selected().unwrap().id, "2");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Select)
        );
    }

    #[test]
    fn failed_revalidation_keeps_cached_selection_available() {
        let mut app = App::new(false);
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(LoadedAccounts::Cached(subscriptions(), vec![]))
            .unwrap();
        app.receive_accounts(&receiver);
        app.append_query("hr");
        app.next();
        sender
            .send(LoadedAccounts::Failed("sign in required".to_owned()))
            .unwrap();
        app.receive_accounts(&receiver);
        assert!(!app.loading);
        assert!(app.cached);
        assert!(app.load_error.is_some());
        assert_eq!(app.selected().unwrap().id, "1");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Select)
        );
    }

    #[test]
    fn keeps_the_visible_end_of_a_long_query() {
        assert_eq!(trailing_width("123456", 4), "3456");
        assert_eq!(trailing_width("a界b", 3), "界b");
    }
}
