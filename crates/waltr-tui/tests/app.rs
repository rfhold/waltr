//! App-level rendering and input tests.

use std::time::{Duration, Instant};

use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Rect,
    style::{Color, Modifier, Style},
};
use waltr_client::{
    AuthAttemptSnapshot, AuthenticationFailureSnapshot, CatalogMatchSourceView, ClientAuthTarget,
    ClientCommand, ClientManualRoutineDefinition, ClientOperationView,
    ClientRoutineInvocationDetail, ClientRoutineInvocationStatus, ClientRoutineInvocationSummary,
    ClientRoutineTriggerKind, ClientSnapshot, ClientTimelineDirection, ConfigView,
    InferenceProviderSettingsView, InferenceSettingsMatchView, InferenceSettingsView,
    McpAuthorizationContextSnapshot, McpServerTypeView, McpServerView, ModelCapabilitiesView,
    ModelLimitsView, ModelOptionsView, ModelTypeView, ModelView, OAuthJwtMetadataView,
    OAuthProviderView, OAuthTokenFieldView, OAuthTokenRequestStyleView, OAuthUrlParamView,
    OperationSnapshot, ProviderKindView, ProviderView, RollbackTargetView, RoutineBudgets,
    RoutineContract, RoutineDetailSnapshot, RoutineEntrypoint, RoutineJson, RoutineOperation,
    RoutineOperationStatus, RoutineOwnership, RoutineRequest, RoutineRequestState,
    RoutineRequestStatus, RoutineRevision, RoutineStartContext, RoutineTextResult,
    SessionCoordinationReasonView, SessionPhaseView, SessionProgressPhaseView,
    SessionPromptDispositionView, SessionRunStageView,
    SessionTimelineView, TimelineEventView,
};
use waltr_tui::{
    app::{App, AppAction, AppScreen},
    event::AppEvent,
    input::InputEvent,
    screens::{ConfigTab, SectionAction, SessionsSection, draw_sessions},
    theme::{StatusRole, THEME},
    ui,
};

mod support;

use support::app::*;

include!("app/basic.rs");
include!("app/sessions.rs");
include!("app/routines.rs");
include!("app/session_data.rs");
include!("app/config.rs");
