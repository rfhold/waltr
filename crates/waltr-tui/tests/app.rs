//! App-level rendering and input tests.

use std::time::{Duration, Instant};

use waltr_client::{
    AuthAttemptSnapshot, AuthenticationFailureSnapshot, CatalogMatchSourceView, ClientAuthTarget,
    ClientCommand, ClientOperationView, ClientSnapshot, ClientTimelineDirection, ConfigView,
    McpAuthorizationContextSnapshot, McpServerTypeView, McpServerView, ModelCapabilitiesView,
    ModelLimitsView, ModelOptionsView, ModelTypeView, ModelView, OAuthJwtMetadataView,
    OAuthProviderView, OAuthTokenFieldView, OAuthTokenRequestStyleView, OAuthUrlParamView,
    OperationSnapshot, ProviderKindView, ProviderView, RollbackTargetView, RuntimeConfigMatchView,
    SessionMcpServerRuntimeView, SessionPhaseView, SessionProviderRuntimeView,
    SessionRuntimeConfigView, SessionTimelineView, TimelineContentView, TimelineEventKindView,
    TimelineEventView, TimelineStatusView,
};
use waltr_tui::{
    app::{App, AppAction, AppScreen},
    event::AppEvent,
    input::InputEvent,
    screens::{ConfigTab, SectionAction, SessionsSection, draw_sessions},
    theme::{StatusRole, THEME},
    ui,
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Rect,
    style::{Color, Modifier, Style},
};

mod support;

use support::app::*;

include!("app/basic.rs");
include!("app/sessions.rs");
include!("app/config.rs");
