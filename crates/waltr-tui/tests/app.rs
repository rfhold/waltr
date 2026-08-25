//! App-level rendering and input tests.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use waltr_client::{
    AuthAttemptSnapshot, AuthenticationFailureSnapshot, CatalogMatchSourceView, ClientAuthTarget,
    ClientCommand, ClientManualRoutineDefinition, ClientOperationView, ClientRoutineFailure,
    ClientRoutineInputDefinition, ClientRoutineInvocationDetail, ClientRoutineInvocationStatus,
    ClientRoutineInvocationSummary, ClientRoutineResult, ClientRoutineResultDefinition,
    ClientRoutineScope, ClientRoutineTask, ClientRoutineTaskStatus, ClientRoutineTriggerKind,
    ClientRoutineValue, ClientRoutineValueType, ClientSnapshot, ClientTimelineDirection, ConfigView,
    InferenceProviderSettingsView, InferenceSettingsMatchView, InferenceSettingsView,
    McpAuthorizationContextSnapshot, McpServerTypeView, McpServerView, ModelCapabilitiesView,
    ModelLimitsView, ModelOptionsView, ModelTypeView, ModelView, OAuthJwtMetadataView,
    OAuthProviderView, OAuthTokenFieldView, OAuthTokenRequestStyleView, OAuthUrlParamView,
    OperationSnapshot, ProviderKindView, ProviderView, RollbackTargetView, RoutineDetailSnapshot,
    SessionPhaseView, SessionSuspensionKindView, SessionTimelineView, TimelineContentView,
    TimelineEventKindView, TimelineEventView, TimelineStatusView,
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
include!("app/routines.rs");
include!("app/session_data.rs");
include!("app/config.rs");
