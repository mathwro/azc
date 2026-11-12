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

enum InitialLoad {
    Accounts { refresh: bool },
    TenantMetadata,
    Ready,
}

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

pub fn choose_from_query(
    azure: AzureCli,
    subscriptions: Vec<Subscription>,
    descriptions: Option<Vec<TenantDescription>>,
    query: String,
    tenant_mode: bool,
    shortlist: Option<Vec<usize>>,
) -> Result<Option<Subscription>, Box<dyn Error>> {
    let initial_load = if descriptions.is_some() {
        InitialLoad::Ready
    } else {
        InitialLoad::TenantMetadata
    };
    let app = App::from_query(subscriptions, descriptions, query, tenant_mode, shortlist);
    choose_with_app(load_in_background(azure, initial_load), app)
}

fn choose(
    azure: AzureCli,
    refresh: bool,
    tenant_mode: bool,
) -> Result<Option<Subscription>, Box<dyn Error>> {
    choose_with_app(
        load_in_background(azure, InitialLoad::Accounts { refresh }),
        App::for_accounts(tenant_mode, refresh),
    )
}

fn choose_with_app(
    loader: AccountLoader,
    app: App,
) -> Result<Option<Subscription>, Box<dyn Error>> {
    let mut terminal = ratatui::try_init()?;
    let result = run(&mut terminal, loader, app);
    let restore_result = ratatui::try_restore();

    match (result, restore_result) {
        (Ok(selection), Ok(())) => Ok(selection),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(Box::new(error)),
    }
}

fn load_in_background(azure: AzureCli, initial_load: InitialLoad) -> AccountLoader {
    let (sender, accounts) = mpsc::channel();
    let (request, requests) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let connected = match initial_load {
            InitialLoad::Accounts { refresh } => load_accounts(&azure, refresh, &sender),
            InitialLoad::TenantMetadata => sender
                .send(LoadedAccounts::Tenants(
                    azure.list_tenants().unwrap_or_default(),
                ))
                .is_ok(),
            InitialLoad::Ready => true,
        };
        if !connected {
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
    let cache = Cache::for_azure(azure.executable());
    let mut previous_descriptions = Vec::new();
    if !refresh && let Some(snapshot) = cache.as_ref().and_then(Cache::load) {
        previous_descriptions.clone_from(&snapshot.descriptions);
        if sender
            .send(LoadedAccounts::Cached(
                snapshot.subscriptions,
                snapshot.descriptions,
            ))
            .is_err()
        {
            return false;
        }
    }
    thread::scope(|scope| {
        // Overlap read-only tenant metadata discovery with `az account list`.
        // Refresh loads wait for `az account list --refresh` before reading tenants.
        let tenants = (!refresh).then(|| scope.spawn(|| azure.list_tenants()));
        let subscriptions: Arc<Vec<Subscription>> = match azure.list_subscriptions(refresh) {
            Ok(subscriptions) => subscriptions.into(),
            Err(error) => {
                return sender
                    .send(LoadedAccounts::Failed(error.to_string()))
                    .is_ok();
            }
        };
        // Save and publish subscriptions before waiting for tenant metadata, keeping
        // previous tenant labels in case the user switches before metadata arrives.
        if let Some(cache) = &cache {
            let _ = cache.save(&subscriptions, &previous_descriptions);
        }
        if sender
            .send(LoadedAccounts::Subscriptions(Arc::clone(&subscriptions)))
            .is_err()
        {
            return false;
        }
        let descriptions = match tenants {
            Some(handle) => handle
                .join()
                .unwrap_or_else(|payload| std::panic::resume_unwind(payload)),
            None => azure.list_tenants(),
        }
        .unwrap_or_default();
        if let Some(cache) = &cache {
            let _ = cache.save(&subscriptions, &descriptions);
        }
        sender.send(LoadedAccounts::Tenants(descriptions)).is_ok()
    })
}

fn poll_timeout(app: &App) -> Duration {
    if app.is_loading() && !app.has_accounts {
        Duration::from_millis(5)
    } else {
        Duration::from_millis(50)
    }
}

fn run(
    terminal: &mut DefaultTerminal,
    loader: AccountLoader,
    mut app: App,
) -> Result<Option<Subscription>, Box<dyn Error>> {
    let mut dirty = true;
    loop {
        let outcome = app.receive_accounts(&loader.accounts);
        dirty |= outcome.changed;
        if outcome.action == Some(Action::Select) {
            return Ok(app.selected().cloned());
        }
        if dirty {
            terminal.draw(|frame| render(frame, &mut app))?;
            dirty = false;
        }

        if event::poll(poll_timeout(&app))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    dirty = true;
                    if let Some(action) = app.handle_key(key) {
                        match action {
                            Action::Cancel => return Ok(None),
                            Action::Select => return Ok(app.selected().cloned()),
                            Action::Refresh => {
                                if loader.refresh.try_send(()).is_err() {
                                    app.load_state = LoadState::Idle;
                                    app.load_error =
                                        Some("subscription loader stopped unexpectedly".to_owned());
                                }
                            }
                        }
                    }
                }
                Event::Paste(text) => {
                    app.append_query(&text);
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoadState {
    Idle,
    LoadingSubscriptions,
    /// `Ctrl+R` queued a server refresh behind an in-flight load; superseded
    /// messages are ignored until `RefreshStarted` arrives.
    QueuedRefresh,
    /// A server refresh's `az account list --refresh` has not returned yet.
    /// Azure CLI writes its stale active subscription back when it finishes,
    /// so switching must wait for it.
    RefreshingSubscriptions,
    LoadingTenants {
        refreshing: bool,
    },
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
    load_state: LoadState,
    /// Subscription ID chosen with Enter while a refresh listing is in flight.
    pending_switch: Option<String>,
    load_error: Option<String>,
    tenant_mode: bool,
    tenant_scope: Option<String>,
    scope_to_current_tenant: bool,
    shortlist: Option<Vec<usize>>,
}

impl App {
    fn new(tenant_mode: bool) -> Self {
        Self::for_accounts(tenant_mode, false)
    }

    fn for_accounts(tenant_mode: bool, refresh: bool) -> Self {
        Self {
            subscriptions: Arc::new(Vec::new()),
            descriptions: Vec::new(),
            has_accounts: false,
            cached: false,
            tenants: Vec::new(),
            filtered: Vec::new(),
            query: String::new(),
            list_state: ListState::default(),
            load_state: if refresh {
                LoadState::RefreshingSubscriptions
            } else {
                LoadState::LoadingSubscriptions
            },
            pending_switch: None,
            load_error: None,
            tenant_mode,
            tenant_scope: None,
            scope_to_current_tenant: true,
            shortlist: None,
        }
    }

    fn from_query(
        subscriptions: Vec<Subscription>,
        descriptions: Option<Vec<TenantDescription>>,
        query: String,
        tenant_mode: bool,
        shortlist: Option<Vec<usize>>,
    ) -> Self {
        let mut app = Self::new(tenant_mode);
        app.scope_to_current_tenant = false;
        app.load_state = if descriptions.is_none() {
            LoadState::LoadingTenants { refreshing: false }
        } else {
            LoadState::Idle
        };
        app.descriptions = descriptions.unwrap_or_default();
        app.query = query;
        app.shortlist = shortlist.map(|mut indices| {
            indices.sort_unstable();
            indices
        });
        app.replace_subscriptions(subscriptions.into());
        app
    }

    fn is_loading(&self) -> bool {
        self.load_state != LoadState::Idle
    }

    fn is_refreshing(&self) -> bool {
        matches!(
            self.load_state,
            LoadState::QueuedRefresh
                | LoadState::RefreshingSubscriptions
                | LoadState::LoadingTenants { refreshing: true }
        )
    }

    fn is_refresh_listing(&self) -> bool {
        matches!(
            self.load_state,
            LoadState::QueuedRefresh | LoadState::RefreshingSubscriptions
        )
    }

    fn receive_accounts(&mut self, receiver: &Receiver<LoadedAccounts>) -> PollOutcome {
        if !self.is_loading() {
            return PollOutcome {
                changed: false,
                action: None,
            };
        }

        let mut changed = false;
        loop {
            match receiver.try_recv() {
                Ok(LoadedAccounts::RefreshStarted) => {
                    self.load_state = LoadState::RefreshingSubscriptions;
                    changed = true;
                }
                // The initial load may finish after Ctrl+R queues a server refresh.
                Ok(_) if self.load_state == LoadState::QueuedRefresh => {}
                Ok(LoadedAccounts::Cached(subscriptions, descriptions)) => {
                    self.descriptions = descriptions;
                    self.replace_subscriptions(subscriptions.into());
                    self.cached = true;
                    changed = true;
                }
                Ok(LoadedAccounts::Subscriptions(subscriptions)) => {
                    let refreshing = self.is_refreshing();
                    self.replace_subscriptions(subscriptions);
                    self.cached = false;
                    self.load_state = LoadState::LoadingTenants { refreshing };
                    changed = true;
                    let switch = self.take_pending_switch();
                    if switch.is_some() {
                        return PollOutcome {
                            changed,
                            action: switch,
                        };
                    }
                }
                Ok(LoadedAccounts::Tenants(descriptions)) => {
                    self.descriptions = descriptions;
                    self.replace_subscriptions(Arc::clone(&self.subscriptions));
                    self.load_state = LoadState::Idle;
                    changed = true;
                    break;
                }
                // The refreshing Azure CLI process has exited, so it can no longer
                // overwrite the context; Azure CLI validates the ID when switching.
                Ok(LoadedAccounts::Failed(error)) => {
                    self.load_state = LoadState::Idle;
                    self.load_error = Some(error);
                    return PollOutcome {
                        changed: true,
                        action: self.take_pending_switch(),
                    };
                }
                Err(TryRecvError::Disconnected) => {
                    self.load_state = LoadState::Idle;
                    self.pending_switch = None;
                    self.load_error = Some("subscription loader stopped unexpectedly".to_owned());
                    changed = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        PollOutcome {
            changed,
            action: None,
        }
    }

    /// Completes a deferred Enter if its subscription is still highlighted.
    fn take_pending_switch(&mut self) -> Option<Action> {
        let id = self.pending_switch.take()?;
        self.selected()
            .is_some_and(|subscription| subscription.id == id)
            .then_some(Action::Select)
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
                    self.filtered
                        .iter()
                        .position(|&index| self.tenants[index].id.eq_ignore_ascii_case(id))
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
        // Any key other than Enter abandons a deferred switch.
        self.pending_switch = None;
        // Windows reports AltGr as Ctrl+Alt with the produced character, and
        // that cannot be told apart from a literal Ctrl+Alt chord, so keep it
        // searchable rather than treating it as an Alt or Ctrl shortcut.
        if let KeyCode::Char(character) = key.code
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && !character.is_control()
        {
            self.query.push(character);
            self.refresh_filter();
            return None;
        }
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
                KeyCode::Char('r') if self.shortlist.is_none() && !self.is_refreshing() => {
                    self.load_state = LoadState::QueuedRefresh;
                    self.load_error = None;
                    Some(Action::Refresh)
                }
                KeyCode::Char('t') if self.shortlist.is_none() => {
                    self.tenant_mode = !self.tenant_mode;
                    self.query.clear();
                    self.refresh_filter();
                    None
                }
                KeyCode::Char('b') if self.shortlist.is_some() => {
                    let selected = self
                        .list_state
                        .selected()
                        .and_then(|selected| self.filtered.get(selected))
                        .copied();
                    self.shortlist = None;
                    self.query.clear();
                    self.refresh_filter();
                    if let Some(index) = selected {
                        self.list_state.select(
                            self.filtered
                                .iter()
                                .position(|&candidate| candidate == index),
                        );
                    }
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
                if self.tenant_mode && self.shortlist.is_none() {
                    self.tenant_scope = Some(self.selected()?.tenant_id.trim().to_owned());
                    self.tenant_mode = false;
                    self.query.clear();
                    self.refresh_filter();
                    None
                } else if self.is_refresh_listing() {
                    self.pending_switch =
                        self.selected().map(|subscription| subscription.id.clone());
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
        self.pending_switch = None;
        self.query
            .extend(text.chars().filter(|character| !character.is_control()));
        self.refresh_filter();
    }

    fn scoped_tenant_id(&self) -> Option<&str> {
        self.tenant_scope.as_deref().or_else(|| {
            if !self.scope_to_current_tenant {
                return None;
            }
            self.tenants
                .iter()
                .find(|tenant| tenant.is_current)
                .map(|tenant| tenant.id.as_str())
        })
    }

    fn refresh_filter(&mut self) {
        self.filtered = if self.tenant_mode {
            ranked_tenant_indices(&self.query, &self.tenants)
        } else {
            let mut indices = ranked_indices(&self.query, &self.subscriptions);
            if let Some(tenant_id) = self.scoped_tenant_id() {
                indices.retain(|&index| {
                    self.subscriptions[index]
                        .tenant_id
                        .trim()
                        .eq_ignore_ascii_case(tenant_id)
                });
            }
            indices
        };
        if let Some(shortlist) = &self.shortlist {
            self.filtered
                .retain(|index| shortlist.binary_search(index).is_ok());
        }
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
        self.list_state.select(self.filtered.len().checked_sub(1));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Cancel,
    Select,
    Refresh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PollOutcome {
    changed: bool,
    action: Option<Action>,
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

fn current_tenant_label(app: &App) -> String {
    app.tenants
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
        )
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
                    Span::styled(subscription.name.as_str(), name_style),
                    Span::styled(
                        format!("  {}", subscription.id),
                        Style::new().fg(Color::DarkGray),
                    ),
                ]))
            })
            .collect::<Vec<_>>()
    };

    let tenant_scope = app.scoped_tenant_id();
    let list_name = if app.tenant_mode {
        "Tenants".to_owned()
    } else if let Some(id) = tenant_scope {
        let label = app
            .tenants
            .iter()
            .find(|tenant| tenant.id.eq_ignore_ascii_case(id))
            .map_or_else(|| id.to_owned(), Tenant::label);
        format!("Subscriptions in {label}")
    } else {
        "Subscriptions".to_owned()
    };
    let title = if app.is_loading() {
        format!(
            " {} · {} ",
            list_name,
            if app.is_refreshing() && app.cached {
                "cached · refreshing Azure…"
            } else if app.is_refreshing() {
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
        let error = format_single_line(error);
        format!(
            " {}Azure CLI error · {error} ",
            if app.cached { "cached · " } else { "" }
        )
    } else {
        let current_tenant = current_tenant_label(app);
        let total = if app.tenant_mode {
            app.tenants.len()
        } else if let Some(id) = tenant_scope {
            app.tenants
                .iter()
                .find(|tenant| tenant.id.eq_ignore_ascii_case(id))
                .map_or(0, |tenant| tenant.subscription_count)
        } else {
            app.subscriptions.len()
        };
        format!(
            " {} · {} of {} · current tenant {} ",
            list_name,
            app.filtered.len(),
            total,
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

    if !app.is_loading() && app.load_error.is_none() && app.filtered.is_empty() {
        let message = if app.tenant_mode {
            if app.tenants.is_empty() {
                "No tenants with enabled subscriptions found"
            } else {
                "No matching tenants"
            }
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
    if let Some(subscription) = app.pending_switch.as_ref().and(app.selected()) {
        let status = format!(
            " Waiting for Azure refresh to switch to {} · Esc cancel ",
            subscription.name
        );
        frame.render_widget(
            Paragraph::new(status).style(Style::new().fg(Color::Yellow)),
            area,
        );
        return;
    }
    if app.shortlist.is_some() {
        let status = format!(
            " Shortlist · {} · Ctrl+B full picker · Esc cancel ",
            if app.selected().is_some() {
                "Enter switch"
            } else {
                "Type to search"
            }
        );
        frame.render_widget(
            Paragraph::new(status).style(Style::new().fg(Color::DarkGray)),
            area,
        );
        return;
    }
    let current_tenant = current_tenant_label(app);
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

fn format_single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let mut words = line
            .split(|character: char| character.is_whitespace() || character.is_control())
            .filter(|word| !word.is_empty());
        let Some(first) = words.next() else {
            continue;
        };
        if !out.is_empty() {
            out.push_str(" — ");
        }
        out.push_str(first);
        for word in words {
            out.push(' ');
            out.push_str(word);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Action, App, LoadState, LoadedAccounts, PollOutcome, poll_timeout, render, trailing_width,
    };
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
            })
            .collect()
    }

    fn loaded_app() -> App {
        let mut app = App::new(false);
        app.load_state = LoadState::Idle;
        app.replace_subscriptions(subscriptions().into());
        app
    }

    fn tenant_query_app(query: &str, shortlist: Option<Vec<usize>>) -> App {
        let subscriptions = serde_json::from_str(
            r#"[{"id":"a-first","name":"Production First","tenantId":"tenant-z"},
                {"id":"a-current","name":"Production Current","tenantId":"tenant-z","isDefault":true},
                {"id":"b-first","name":"Development First","tenantId":"tenant-a"},
                {"id":"b-last","name":"Development Last","tenantId":"tenant-a"},
                {"id":"outside","name":"Other","tenantId":"tenant-other"}]"#,
        )
        .unwrap();
        let descriptions = serde_json::from_str(
            r#"[{"tenantId":"tenant-z","displayName":"North Production","defaultDomain":null},
                {"tenantId":"tenant-a","displayName":null,"defaultDomain":"development.example"},
                {"tenantId":"tenant-a","displayName":"North Development"},
                {"tenantId":"tenant-other","displayName":"Alpha Outside"}]"#,
        )
        .unwrap();
        App::from_query(
            subscriptions,
            Some(descriptions),
            query.to_owned(),
            true,
            shortlist,
        )
    }

    #[test]
    fn direct_query_recovery_keeps_cross_tenant_choices_available() {
        for shortlist in [None, Some(vec![1, 2])] {
            let subscriptions = serde_json::from_str(
                r#"[{"id":"a","name":"Finance","tenantId":"tenant-a","isDefault":true},
                    {"id":"b","name":"Payments Development","tenantId":"tenant-b"},
                    {"id":"c","name":"Payments Production","tenantId":"tenant-c"}]"#,
            )
            .unwrap();
            let mut app = App::from_query(
                subscriptions,
                Some(vec![]),
                "payments".to_owned(),
                false,
                shortlist,
            );
            assert_eq!(app.filtered, vec![1, 2]);
            app.next();
            assert_eq!(app.selected().unwrap().id, "c");
            app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
            assert_eq!(app.selected().unwrap().id, "c");
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Some(Action::Select)
            );
        }
    }

    #[test]
    fn shortlist_never_exposes_lower_matches_or_outsiders_after_clearing() {
        let mut subscriptions = subscriptions();
        subscriptions.push(Subscription {
            id: "lower".to_owned(),
            name: "Archive HR".to_owned(),
            tenant_id: "tenant".to_owned(),
            is_default: false,
        });
        let mut app = App::from_query(
            subscriptions,
            None,
            "hr".to_owned(),
            false,
            Some(vec![2, 1]),
        );
        assert_eq!(app.filtered, vec![2, 1]);

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(app.query.is_empty());
        assert_eq!(app.filtered, vec![2, 1]);
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.selected().unwrap().id, "1");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected().unwrap().id, "2");
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(LoadedAccounts::Tenants(vec![])).unwrap();
        app.receive_accounts(&receiver);
        assert!(!app.is_loading());
        assert_eq!(app.filtered, vec![2, 1]);
        assert_eq!(app.selected().unwrap().id, "2");
        app.append_query("Finance");
        assert!(app.selected().is_none());
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(app.selected().is_none());
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.append_query("Production");
        assert_eq!(app.selected().unwrap().id, "1");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Select)
        );
    }

    #[test]
    fn shortlist_ignores_refresh_and_mode_toggle_but_still_cancels() {
        for tenant_mode in [false, true] {
            let mut app = App::from_query(
                subscriptions(),
                Some(vec![]),
                String::new(),
                tenant_mode,
                Some(vec![0]),
            );
            for key in ['r', 't', 'r'] {
                assert_eq!(
                    app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL)),
                    None
                );
            }
            assert!(!app.is_loading());
            assert!(!app.is_refreshing());
            assert_eq!(app.tenant_mode, tenant_mode);
            assert_eq!(app.selected().unwrap().id, "0");
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                Some(Action::Cancel)
            );
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                Some(Action::Cancel)
            );
        }
    }

    #[test]
    fn leaving_subscription_shortlist_preserves_id_and_restores_controls() {
        let mut app = App::from_query(
            subscriptions(),
            Some(vec![]),
            "hr".to_owned(),
            false,
            Some(vec![1, 2]),
        );
        app.next();
        assert_eq!(app.selected().unwrap().id, "1");

        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));

        assert!(app.query.is_empty());
        assert!(!app.tenant_mode);
        assert_eq!(app.filtered.len(), 3);
        assert_eq!(app.selected().unwrap().id, "1");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(Action::Refresh)
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            None
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(app.tenant_mode);
    }

    #[test]
    fn tenant_shortlist_enter_selects_current_or_first_subscription_directly() {
        let mut app = tenant_query_app("north", Some(vec![0, 1]));
        assert_eq!(app.selected().unwrap().id, "a-current");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Select)
        );
        assert!(app.tenant_mode);
        app.next();
        assert_eq!(app.selected().unwrap().id, "b-first");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Select)
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.filtered.len(), 2);
        app.append_query("outside");
        assert!(app.selected().is_none());
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn leaving_tenant_shortlist_preserves_tenant_and_restores_drilldown() {
        let mut app = tenant_query_app("north", Some(vec![0, 1]));
        app.next();
        assert_eq!(app.selected().unwrap().tenant_id, "tenant-a");

        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));

        assert!(app.query.is_empty());
        assert!(app.tenant_mode);
        assert_eq!(app.filtered.len(), 3);
        assert_eq!(app.selected().unwrap().tenant_id, "tenant-a");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert!(!app.tenant_mode);
        assert_eq!(app.filtered.len(), 2);
        app.next();
        assert_eq!(app.selected().unwrap().id, "b-last");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Select)
        );
    }

    #[test]
    fn unmatched_subscription_query_can_be_corrected_before_metadata_arrives() {
        let mut app = App::from_query(subscriptions(), None, "financx".to_owned(), false, None);
        assert!(app.is_loading());
        assert_eq!(app.query, "financx");
        assert!(app.selected().is_none());
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(app.selected().unwrap().id, "0");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Select)
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(app.query, "finance");
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(LoadedAccounts::Tenants(vec![])).unwrap();
        app.receive_accounts(&receiver);
        assert!(!app.is_loading());
        assert_eq!(app.selected().unwrap().id, "0");
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.filtered.len(), 3);
    }

    #[test]
    fn unmatched_tenant_query_can_be_corrected_and_drills_down_normally() {
        let mut app = tenant_query_app("north missing", None);
        assert_eq!(app.query, "north missing");
        assert!(app.selected().is_none());
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.append_query("development");
        assert_eq!(app.selected().unwrap().tenant_id, "tenant-a");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        assert!(!app.tenant_mode);
        assert_eq!(app.filtered.len(), 2);
        app.next();
        assert_eq!(app.selected().unwrap().id, "b-last");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Select)
        );
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

        assert!(app.is_loading());
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

        assert!(app.is_loading());
        assert!(app.load_error.is_none());
        assert_eq!(app.query, "hr");
        assert_eq!(app.selected().unwrap().id, "2");

        sender.send(LoadedAccounts::RefreshStarted).unwrap();
        sender
            .send(LoadedAccounts::Subscriptions(subscriptions().into()))
            .unwrap();
        sender.send(LoadedAccounts::Tenants(vec![])).unwrap();
        app.receive_accounts(&receiver);
        assert!(!app.is_loading());
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

        assert!(!app.is_loading());
        assert!(app.load_error.is_some());
        assert_eq!(app.query, "hr");
        assert_eq!(app.selected().unwrap().id, "1");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(Action::Refresh)
        );
        assert!(app.is_loading());
        assert!(app.load_error.is_none());
    }

    fn footer(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 6)).unwrap();
        let frame = terminal.draw(|frame| render(frame, app)).unwrap();
        frame.buffer.content()[500..]
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        app.handle_key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn enter_during_refresh_listing_switches_after_refreshed_subscriptions() {
        let mut app = loaded_app();
        app.append_query("hr");
        app.next();
        assert_eq!(
            press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL),
            Some(Action::Refresh)
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(LoadedAccounts::RefreshStarted).unwrap();
        assert_eq!(app.receive_accounts(&receiver).action, None);

        assert_eq!(press(&mut app, KeyCode::Enter, KeyModifiers::NONE), None);
        assert!(footer(&mut app).contains("Waiting for Azure refresh to switch to HR Production"));
        assert_eq!(app.receive_accounts(&receiver).action, None);

        let mut refreshed = subscriptions();
        refreshed.reverse();
        sender
            .send(LoadedAccounts::Subscriptions(refreshed.into()))
            .unwrap();
        sender.send(LoadedAccounts::Tenants(vec![])).unwrap();
        assert_eq!(app.receive_accounts(&receiver).action, Some(Action::Select));
        assert_eq!(app.selected().unwrap().id, "1");
    }

    #[test]
    fn enter_during_queued_refresh_waits_past_superseded_startup_results() {
        let mut app = App::new(false);
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(LoadedAccounts::Cached(subscriptions(), vec![]))
            .unwrap();
        app.receive_accounts(&receiver);
        assert_eq!(
            press(&mut app, KeyCode::Enter, KeyModifiers::NONE),
            Some(Action::Select)
        );
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(press(&mut app, KeyCode::Enter, KeyModifiers::NONE), None);

        sender
            .send(LoadedAccounts::Subscriptions(subscriptions().into()))
            .unwrap();
        sender.send(LoadedAccounts::Tenants(vec![])).unwrap();
        assert_eq!(app.receive_accounts(&receiver).action, None);
        sender.send(LoadedAccounts::RefreshStarted).unwrap();
        assert_eq!(app.receive_accounts(&receiver).action, None);
        sender
            .send(LoadedAccounts::Subscriptions(subscriptions().into()))
            .unwrap();
        assert_eq!(app.receive_accounts(&receiver).action, Some(Action::Select));
        assert_eq!(app.selected().unwrap().id, "0");
    }

    #[test]
    fn deferred_switch_clears_when_refresh_removes_the_subscription() {
        let mut app = loaded_app();
        app.append_query("hr");
        app.next();
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(press(&mut app, KeyCode::Enter, KeyModifiers::NONE), None);
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(LoadedAccounts::RefreshStarted).unwrap();
        let refreshed: Vec<_> = subscriptions()
            .into_iter()
            .filter(|subscription| subscription.id != "1")
            .collect();
        sender
            .send(LoadedAccounts::Subscriptions(refreshed.into()))
            .unwrap();

        assert_eq!(app.receive_accounts(&receiver).action, None);
        assert!(app.selected().is_none());
        assert!(!footer(&mut app).contains("Waiting for Azure refresh"));
        assert_eq!(press(&mut app, KeyCode::Enter, KeyModifiers::NONE), None);
        // Tenant metadata is still loading, but it never delays a switch.
        app.next();
        assert!(app.is_loading());
        assert_eq!(app.selected().unwrap().id, "2");
        assert_eq!(
            press(&mut app, KeyCode::Enter, KeyModifiers::NONE),
            Some(Action::Select)
        );
    }

    #[test]
    fn deferred_switch_is_abandoned_by_cancel_or_further_input() {
        for (code, modifiers) in [
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut app = loaded_app();
            press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
            assert_eq!(press(&mut app, KeyCode::Enter, KeyModifiers::NONE), None);
            assert_eq!(press(&mut app, code, modifiers), Some(Action::Cancel));
        }

        let mut app = loaded_app();
        app.append_query("hr");
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(press(&mut app, KeyCode::Enter, KeyModifiers::NONE), None);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.selected().unwrap().id, "1");
        assert_eq!(press(&mut app, KeyCode::Enter, KeyModifiers::NONE), None);
        app.append_query(" production");
        assert_eq!(app.selected().unwrap().id, "1");
        assert!(!footer(&mut app).contains("Waiting for Azure refresh"));

        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(LoadedAccounts::RefreshStarted).unwrap();
        sender
            .send(LoadedAccounts::Subscriptions(subscriptions().into()))
            .unwrap();
        sender.send(LoadedAccounts::Tenants(vec![])).unwrap();
        assert_eq!(app.receive_accounts(&receiver).action, None);
        assert!(!app.is_loading());
        assert_eq!(app.selected().unwrap().id, "1");
    }

    #[test]
    fn failed_refresh_completes_the_deferred_switch() {
        let mut app = loaded_app();
        app.append_query("hr");
        app.next();
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(LoadedAccounts::RefreshStarted).unwrap();
        app.receive_accounts(&receiver);
        assert_eq!(press(&mut app, KeyCode::Enter, KeyModifiers::NONE), None);
        sender
            .send(LoadedAccounts::Failed("refresh failed".to_owned()))
            .unwrap();

        assert_eq!(app.receive_accounts(&receiver).action, Some(Action::Select));
        assert!(app.load_error.is_some());
        assert_eq!(app.selected().unwrap().id, "1");
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
    fn altgr_characters_reported_as_ctrl_alt_become_search_text() {
        let mut app = loaded_app();
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;

        for character in ['ł', '@', 'j'] {
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char(character), altgr)),
                None
            );
        }
        assert_eq!(app.query, "ł@j");

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.query, "");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Cancel)
        );
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
    fn subscription_picker_defaults_to_the_live_context_tenant() {
        let cached: Vec<Subscription> = serde_json::from_str(
            r#"[{"id":"a","name":"Alpha","tenantId":"tenant-a","isDefault":true},
                {"id":"b","name":"Beta","tenantId":"tenant-b"},
                {"id":"c","name":"Gamma","tenantId":" TENANT-B "}]"#,
        )
        .unwrap();
        let mut descriptions = serde_json::from_str(
            r#"[{"tenantId":"tenant-a","displayName":"Tenant A"},
                {"tenantId":"tenant-b"}]"#,
        )
        .unwrap();
        let metadata: Vec<TenantDescription> = serde_json::from_str(
            r#"[{"tenantId":"tenant-b","displayName":"Tenant B"},
                {"tenantId":"tenant-b","defaultDomain":"b.example"}]"#,
        )
        .unwrap();
        AzureCli::merge_tenant_metadata(&mut descriptions, &metadata);
        let mut live = cached.clone();
        live[0].is_default = false;
        live[1].is_default = true;
        let mut app = App::new(false);
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(LoadedAccounts::Cached(cached, descriptions.clone()))
            .unwrap();
        app.receive_accounts(&receiver);
        assert_eq!(app.filtered, vec![0]);
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.filtered, vec![0]);

        sender
            .send(LoadedAccounts::Subscriptions(live.into()))
            .unwrap();
        sender.send(LoadedAccounts::Tenants(descriptions)).unwrap();
        app.receive_accounts(&receiver);
        assert_eq!(app.filtered, vec![1, 2]);
        assert!(app.selected().is_none());
        app.next();
        assert_eq!(app.selected().unwrap().id, "b");
        let mut terminal = Terminal::new(TestBackend::new(160, 10)).unwrap();
        let frame = terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered: String = frame
            .buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Subscriptions in Tenant B (b.example) · 2 of 2"));
        assert!(rendered.contains("Beta"));
        assert!(rendered.contains("Gamma"));
        assert!(!rendered.contains("Alpha"));
        app.append_query("Alpha");
        assert!(app.selected().is_none());
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
        app.load_state = LoadState::Idle;
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
        assert_eq!(app.filtered.len(), 2);
        assert_eq!(app.selected().unwrap().tenant_id.trim(), "other-tenant");
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
        app.load_state = LoadState::LoadingSubscriptions;
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
        assert!(app.is_loading());
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
        assert!(!app.is_loading());
        assert!(app.cached);
        assert!(app.load_error.is_some());
        assert_eq!(app.selected().unwrap().id, "1");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Select)
        );
    }

    #[test]
    fn seeded_refresh_ignores_pending_metadata_and_keeps_the_selected_subscription() {
        let mut app = App::from_query(subscriptions(), None, "hr".to_owned(), false, None);
        app.next();
        assert_eq!(app.selected().unwrap().id, "1");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(Action::Refresh)
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(LoadedAccounts::Tenants(vec![])).unwrap();
        app.receive_accounts(&receiver);
        assert!(app.is_loading());
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            None
        );
        sender.send(LoadedAccounts::RefreshStarted).unwrap();
        sender
            .send(LoadedAccounts::Subscriptions(subscriptions().into()))
            .unwrap();
        sender.send(LoadedAccounts::Tenants(vec![])).unwrap();
        app.receive_accounts(&receiver);
        assert!(!app.is_loading());
        assert_eq!(app.query, "hr");
        assert_eq!(app.selected().unwrap().id, "1");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(Action::Refresh)
        );
    }

    #[test]
    fn seeded_tenant_shortlist_renders_supplied_labels_without_outsiders() {
        let mut app = tenant_query_app("north", Some(vec![0, 1]));
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let frame = terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered: String = frame
            .buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("North Production"));
        assert!(rendered.contains("North Development"));
        assert!(rendered.contains("development.example"));
        assert!(rendered.contains("tenant-z"));
        assert!(rendered.contains("tenant-a"));
        assert!(!rendered.contains("Alpha Outside"));
        assert!(!rendered.contains("tenant-other"));
    }

    #[test]
    fn refresh_startup_defers_enter_before_refresh_started_arrives() {
        let mut app = App::for_accounts(false, true);
        app.replace_subscriptions(subscriptions().into());

        assert!(app.is_loading());
        assert!(app.is_refreshing());
        assert!(app.is_refresh_listing());
        assert_eq!(
            press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL),
            None
        );
        assert_eq!(press(&mut app, KeyCode::Enter, KeyModifiers::NONE), None);
        assert!(footer(&mut app).contains("Waiting for Azure refresh to switch to Finance"));

        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(LoadedAccounts::RefreshStarted).unwrap();
        assert_eq!(app.receive_accounts(&receiver).action, None);
        sender
            .send(LoadedAccounts::Subscriptions(subscriptions().into()))
            .unwrap();
        assert_eq!(app.receive_accounts(&receiver).action, Some(Action::Select));
        assert_eq!(app.selected().unwrap().id, "0");
    }

    #[test]
    fn multiline_load_error_renders_cleanly_on_single_title_row() {
        let mut app = loaded_app();
        app.load_error = Some(
            "az account list failed with exit code 1: WARNING: preview\n\n  ERROR: Please run\taz login  \r\n"
                .to_owned(),
        );

        let mut terminal = Terminal::new(TestBackend::new(140, 6)).unwrap();
        let frame = terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let title_row: String = frame.buffer.content()[420..560]
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(
            title_row.contains(
                "Azure CLI error · az account list failed with exit code 1: WARNING: preview — ERROR: Please run az login"
            ),
            "unexpected title row: {title_row:?}"
        );
        assert!(!title_row.chars().any(char::is_control));
    }

    #[test]
    fn keeps_the_visible_end_of_a_long_query() {
        assert_eq!(trailing_width("123456", 4), "3456");
        assert_eq!(trailing_width("a界b", 3), "界b");
    }
    #[test]
    fn receive_accounts_reports_change_only_when_loader_updates_state() {
        let mut app = App::new(false);
        let (sender, receiver) = std::sync::mpsc::channel();

        // Empty channel while loading must NOT report `changed` (prevents 20 Hz full redraws).
        assert_eq!(
            app.receive_accounts(&receiver),
            PollOutcome {
                changed: false,
                action: None,
            }
        );
        assert_eq!(poll_timeout(&app), std::time::Duration::from_millis(5));

        // Cached snapshot arrives -> changed: true, and poll_timeout relaxes to 50 ms.
        sender
            .send(LoadedAccounts::Cached(subscriptions(), vec![]))
            .unwrap();
        assert_eq!(
            app.receive_accounts(&receiver),
            PollOutcome {
                changed: true,
                action: None,
            }
        );
        assert_eq!(poll_timeout(&app), std::time::Duration::from_millis(50));

        // Subsequent empty poll while still loading -> changed: false.
        assert_eq!(
            app.receive_accounts(&receiver),
            PollOutcome {
                changed: false,
                action: None,
            }
        );

        // Live subscriptions arrive -> changed: true.
        sender
            .send(LoadedAccounts::Subscriptions(subscriptions().into()))
            .unwrap();
        assert_eq!(
            app.receive_accounts(&receiver),
            PollOutcome {
                changed: true,
                action: None,
            }
        );

        // Superseded messages during QueuedRefresh -> changed: false.
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        sender.send(LoadedAccounts::Tenants(vec![])).unwrap();
        assert_eq!(
            app.receive_accounts(&receiver),
            PollOutcome {
                changed: false,
                action: None,
            }
        );

        // RefreshStarted -> changed: true.
        sender.send(LoadedAccounts::RefreshStarted).unwrap();
        assert_eq!(
            app.receive_accounts(&receiver),
            PollOutcome {
                changed: true,
                action: None,
            }
        );

        // Deferred switch completed by Subscriptions -> changed: true, action: Some(Action::Select).
        assert_eq!(press(&mut app, KeyCode::Enter, KeyModifiers::NONE), None);
        sender
            .send(LoadedAccounts::Subscriptions(subscriptions().into()))
            .unwrap();
        assert_eq!(
            app.receive_accounts(&receiver),
            PollOutcome {
                changed: true,
                action: Some(Action::Select),
            }
        );

        // Tenants arrive -> changed: true, transitions to Idle.
        sender.send(LoadedAccounts::Tenants(vec![])).unwrap();
        assert_eq!(
            app.receive_accounts(&receiver),
            PollOutcome {
                changed: true,
                action: None,
            }
        );
        assert!(!app.is_loading());

        // Once Idle, receive_accounts returns changed: false even if messages are queued.
        sender
            .send(LoadedAccounts::Failed("ignored".to_owned()))
            .unwrap();
        assert_eq!(
            app.receive_accounts(&receiver),
            PollOutcome {
                changed: false,
                action: None,
            }
        );

        // Failed while loading -> changed: true.
        let mut failed_app = App::new(false);
        let (fail_tx, fail_rx) = std::sync::mpsc::channel();
        fail_tx
            .send(LoadedAccounts::Failed("boom".to_owned()))
            .unwrap();
        assert_eq!(
            failed_app.receive_accounts(&fail_rx),
            PollOutcome {
                changed: true,
                action: None,
            }
        );

        // Disconnected while loading -> changed: true.
        let mut disc_app = App::new(false);
        let (disc_tx, disc_rx) = std::sync::mpsc::channel();
        drop(disc_tx);
        assert_eq!(
            disc_app.receive_accounts(&disc_rx),
            PollOutcome {
                changed: true,
                action: None,
            }
        );
    }

    #[cfg(unix)]
    mod loader_concurrency {
        use super::super::{LoadedAccounts, load_accounts};
        use crate::azure::AzureCli;
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            path::PathBuf,
            sync::{
                Mutex, MutexGuard,
                atomic::{AtomicU64, Ordering},
                mpsc,
            },
            thread,
            time::Duration,
        };

        static ENV_LOCK: Mutex<()> = Mutex::new(());

        pub(super) struct FakeLoaderEnv {
            pub(super) dir: PathBuf,
            prev_az: Option<std::ffi::OsString>,
            prev_cfg: Option<std::ffi::OsString>,
            _guard: MutexGuard<'static, ()>,
        }

        impl FakeLoaderEnv {
            pub(super) fn new(script: &str) -> Self {
                let guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let dir = std::env::temp_dir().join(format!(
                    "azc-tui-loader-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir_all(&dir).unwrap();
                let script_path = dir.join("az");
                fs::write(&script_path, script).unwrap();
                fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();
                let prev_az = std::env::var_os("AZC_AZ_BIN");
                let prev_cfg = std::env::var_os("AZURE_CONFIG_DIR");
                // SAFETY: Protected by `ENV_LOCK` across tests in this module.
                unsafe {
                    std::env::set_var("AZC_AZ_BIN", &script_path);
                    std::env::set_var("AZURE_CONFIG_DIR", dir.join("azure"));
                }
                Self {
                    dir,
                    prev_az,
                    prev_cfg,
                    _guard: guard,
                }
            }
        }

        impl Drop for FakeLoaderEnv {
            fn drop(&mut self) {
                // SAFETY: Protected by `ENV_LOCK`.
                unsafe {
                    match &self.prev_az {
                        Some(val) => std::env::set_var("AZC_AZ_BIN", val),
                        None => std::env::remove_var("AZC_AZ_BIN"),
                    }
                    match &self.prev_cfg {
                        Some(val) => std::env::set_var("AZURE_CONFIG_DIR", val),
                        None => std::env::remove_var("AZURE_CONFIG_DIR"),
                    }
                }
                let _ = fs::remove_dir_all(&self.dir);
            }
        }

        #[test]
        fn non_refresh_load_overlaps_subscriptions_and_tenants_and_delivers_subscriptions_first() {
            // `account list` waits until `rest` has started (proving `list_tenants()` was
            // spawned concurrently before `list_subscriptions(false)` finished!).
            // Meanwhile `rest` waits until `release-tenants` is touched by the test thread
            // AFTER receiving `LoadedAccounts::Subscriptions` (proving `Subscriptions` is
            // published without waiting for `list_tenants()` to finish!).
            let env = FakeLoaderEnv::new(
                r#"#!/bin/sh
directory=${0%/*}
case "$*" in
  'account list --output json --only-show-errors')
    attempts=0
    while [ ! -f "$directory/tenants-started" ]; do
      attempts=$((attempts + 1))
      if [ "$attempts" -ge 150 ]; then
        echo "list_tenants did not overlap with list_subscriptions" >&2
        exit 1
      fi
      sleep 0.01
    done
    printf '[{"id":"sub-1","name":"Alpha","tenantId":"tenant-a","isDefault":true}]\n'
    ;;
  'rest --method get --url https://management.azure.com/tenants?api-version=2022-12-01 --query value[].{tenantId:tenantId,displayName:displayName,defaultDomain:defaultDomain} --output json')
    touch "$directory/tenants-started"
    attempts=0
    while [ ! -f "$directory/release-tenants" ]; do
      attempts=$((attempts + 1))
      if [ "$attempts" -ge 200 ]; then
        echo "timed out waiting for release-tenants" >&2
        exit 1
      fi
      sleep 0.01
    done
    printf '[{"tenantId":"tenant-a","displayName":"Tenant A","defaultDomain":"a.example"}]\n'
    ;;
  'account list --all --query [].{tenantId:tenantId,displayName:tenantDisplayName,defaultDomain:tenantDefaultDomain} --output json --only-show-errors')
    touch "$directory/tenants-started"
    printf '[{"tenantId":"tenant-a","displayName":"Tenant A","defaultDomain":"a.example"}]\n'
    ;;
  *)
    exit 2
    ;;
esac
"#,
            );
            let azure = AzureCli::from_env();
            let (sender, receiver) = mpsc::channel();
            let dir = env.dir.clone();
            let handle = thread::spawn(move || load_accounts(&azure, false, &sender));

            let first = receiver
                .recv_timeout(Duration::from_secs(3))
                .expect("Subscriptions must arrive while tenant metadata is still blocked");
            match first {
                LoadedAccounts::Subscriptions(subs) => {
                    assert_eq!(subs.len(), 1);
                    assert_eq!(subs[0].id, "sub-1");
                }
                _ => panic!("expected LoadedAccounts::Subscriptions first"),
            }

            fs::write(dir.join("release-tenants"), b"ok").unwrap();

            let second = receiver
                .recv_timeout(Duration::from_secs(3))
                .expect("Tenants must arrive once released");
            match second {
                LoadedAccounts::Tenants(tenants) => {
                    assert_eq!(tenants.len(), 1);
                    assert_eq!(tenants[0].display_name, "Tenant A");
                }
                _ => panic!("expected LoadedAccounts::Tenants second"),
            }
            assert!(handle.join().unwrap());
        }

        #[test]
        fn refresh_load_finishes_subscriptions_before_starting_tenants() {
            // With `refresh = true`, `az account list --refresh` mutates local Azure CLI
            // profile state, so `list_tenants()` MUST NOT start until `--refresh` completes.
            let env = FakeLoaderEnv::new(
                r#"#!/bin/sh
directory=${0%/*}
case "$*" in
  'account list --output json --only-show-errors --refresh')
    sleep 0.05
    printf '[{"id":"sub-1","name":"Alpha","tenantId":"tenant-a","isDefault":true}]\n'
    touch "$directory/refresh-finished"
    ;;
  'rest --method get --url https://management.azure.com/tenants?api-version=2022-12-01 --query value[].{tenantId:tenantId,displayName:displayName,defaultDomain:defaultDomain} --output json' | \
  'account list --all --query [].{tenantId:tenantId,displayName:tenantDisplayName,defaultDomain:tenantDefaultDomain} --output json --only-show-errors')
    if [ ! -f "$directory/refresh-finished" ]; then
      touch "$directory/started-before-refresh"
      echo "tenant query started before refresh finished" >&2
      exit 1
    fi
    printf '[{"tenantId":"tenant-a","displayName":"Tenant A","defaultDomain":"a.example"}]\n'
    ;;
  *)
    exit 2
    ;;
esac
"#,
            );
            let azure = AzureCli::from_env();
            let (sender, receiver) = mpsc::channel();
            assert!(load_accounts(&azure, true, &sender));
            assert!(!env.dir.join("started-before-refresh").exists());

            let messages: Vec<_> = receiver.try_iter().collect();
            assert!(matches!(
                messages.first(),
                Some(LoadedAccounts::RefreshStarted)
            ));
            assert!(matches!(
                messages.get(1),
                Some(LoadedAccounts::Subscriptions(_))
            ));
            assert!(matches!(
                messages.get(2),
                Some(LoadedAccounts::Tenants(tenants)) if tenants.len() == 1 && tenants[0].display_name == "Tenant A"
            ));
        }
    }

    fn subscriptions_1000() -> Vec<Subscription> {
        (0..1_000)
            .map(|index| Subscription {
                id: format!("{index:08x}-0000-4000-8000-{index:012x}"),
                name: format!("Enterprise Cloud Subscription Workload {index:04} - Production"),
                tenant_id: format!("tenant-{:02}", index % 5),
                is_default: index == 0,
            })
            .collect()
    }

    fn median_nanos(mut samples: Vec<u128>) -> u128 {
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    #[test]
    #[ignore]
    fn bench_render_list_1000_subscriptions() {
        use std::hint::black_box;
        use std::time::Instant;

        let descriptions: Vec<TenantDescription> = (0..5)
            .map(|index| TenantDescription {
                tenant_id: format!("tenant-{index:02}"),
                display_name: format!("Enterprise Tenant {index:02}"),
                default_domain: format!("tenant{index:02}.example.com"),
            })
            .collect();

        // 1. Active render() throughput with 1,000 visible/filtered subscriptions
        let mut active_app = App::from_query(
            subscriptions_1000(),
            Some(descriptions.clone()),
            String::new(),
            false,
            None,
        );
        assert_eq!(active_app.filtered.len(), 1_000);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        for _ in 0..20 {
            terminal
                .draw(|frame| render(frame, black_box(&mut active_app)))
                .unwrap();
        }
        let mut render_samples = Vec::with_capacity(100);
        for _ in 0..100 {
            let start = Instant::now();
            terminal
                .draw(|frame| render(frame, black_box(&mut active_app)))
                .unwrap();
            render_samples.push(start.elapsed().as_nanos());
        }
        let render_median_ns = median_nanos(render_samples.clone());
        let render_mean_ns = render_samples.iter().sum::<u128>() / render_samples.len() as u128;

        // 2. Loading render() throughput (e.g. cached 1,000 subscriptions while refreshing/updating)
        let mut loading_app = App::from_query(
            subscriptions_1000(),
            Some(descriptions),
            String::new(),
            false,
            None,
        );
        loading_app.load_state = LoadState::LoadingTenants { refreshing: false };
        for _ in 0..20 {
            terminal
                .draw(|frame| render(frame, black_box(&mut loading_app)))
                .unwrap();
        }
        let mut loading_render_samples = Vec::with_capacity(100);
        for _ in 0..100 {
            let start = Instant::now();
            terminal
                .draw(|frame| render(frame, black_box(&mut loading_app)))
                .unwrap();
            loading_render_samples.push(start.elapsed().as_nanos());
        }
        let loading_render_median_ns = median_nanos(loading_render_samples);

        // 3. Simulated 40 empty background-load polls (2 seconds at 50ms poll interval)
        let (_sender, receiver) = std::sync::mpsc::channel::<LoadedAccounts>();
        for _ in 0..10 {
            let outcome = loading_app.receive_accounts(black_box(&receiver));
            if outcome.changed {
                terminal
                    .draw(|frame| render(frame, black_box(&mut loading_app)))
                    .unwrap();
            }
        }
        let mut idle_poll_samples = Vec::with_capacity(50);
        for _ in 0..50 {
            let start = Instant::now();
            for _ in 0..40 {
                let outcome = loading_app.receive_accounts(black_box(&receiver));
                if outcome.changed {
                    terminal
                        .draw(|frame| render(frame, black_box(&mut loading_app)))
                        .unwrap();
                }
            }
            idle_poll_samples.push(start.elapsed().as_nanos());
        }
        let idle_poll_median_ns = median_nanos(idle_poll_samples.clone());
        let idle_poll_mean_ns =
            idle_poll_samples.iter().sum::<u128>() / idle_poll_samples.len() as u128;

        println!(
            "BENCH render_1000_idle: median={:.2}us mean={:.2}us | render_1000_loading: median={:.2}us | 40_empty_loading_polls: median={:.2}us mean={:.2}us",
            render_median_ns as f64 / 1_000.0,
            render_mean_ns as f64 / 1_000.0,
            loading_render_median_ns as f64 / 1_000.0,
            idle_poll_median_ns as f64 / 1_000.0,
            idle_poll_mean_ns as f64 / 1_000.0,
        );

        #[cfg(unix)]
        {
            use std::fs;

            let env = loader_concurrency::FakeLoaderEnv::new(
                r#"#!/bin/sh
sleep 0.15
case "$*" in
  'account list --output json --only-show-errors')
    printf '[{"id":"sub-1","name":"Alpha","tenantId":"tenant-a","isDefault":true}]\n'
    ;;
  'rest --method get --url https://management.azure.com/tenants?api-version=2022-12-01 --query value[].{tenantId:tenantId,displayName:displayName,defaultDomain:defaultDomain} --output json')
    printf '[{"tenantId":"tenant-a","displayName":"Tenant A","defaultDomain":"a.example"}]\n'
    ;;
  'account list --all --query [].{tenantId:tenantId,displayName:tenantDisplayName,defaultDomain:tenantDefaultDomain} --output json --only-show-errors')
    printf '[{"tenantId":"tenant-a","displayName":"Tenant A","defaultDomain":"a.example"}]\n'
    ;;
  *)
    exit 2
    ;;
esac
"#,
            );
            let azure = AzureCli::from_env();
            // Warmup
            {
                let (tx, _rx) = std::sync::mpsc::channel();
                let _ = fs::remove_dir_all(env.dir.join("azure"));
                super::load_accounts(&azure, false, &tx);
            }
            let mut load_samples = Vec::with_capacity(5);
            for _ in 0..5 {
                let _ = fs::remove_dir_all(env.dir.join("azure"));
                let (tx, rx) = std::sync::mpsc::channel();
                let start = Instant::now();
                assert!(super::load_accounts(black_box(&azure), false, &tx));
                let elapsed = start.elapsed().as_nanos();
                let msgs: Vec<_> = rx.try_iter().collect();
                assert_eq!(msgs.len(), 2);
                load_samples.push(elapsed);
            }
            let load_median_ms = median_nanos(load_samples.clone()) as f64 / 1_000_000.0;
            let load_mean_ms = (load_samples.iter().sum::<u128>() / load_samples.len() as u128)
                as f64
                / 1_000_000.0;
            println!(
                "BENCH load_accounts_150ms_az: median={load_median_ms:.2}ms mean={load_mean_ms:.2}ms"
            );
        }
    }
}
