//! Global Routine reducer transitions.

use std::collections::BTreeSet;

use crate::{
    ClientEffect, ClientRoutineEventKind, ClientRoutineInvocationDetail,
    ClientRoutineInvocationEvent, ClientRoutineInvocationStatus, ClientRoutineInvocationSummary,
    ClientRoutineInvocationWatchUpdate, ClientRoutineStartOutcome, ClientRoutineTask,
    ClientRoutineTaskStatus, ClientState, ClientUpdate, MAX_RETAINED_ROUTINE_DETAILS,
    MAX_RETAINED_ROUTINE_INVOCATIONS, MAX_RETAINED_ROUTINE_STARTS, OperationStatus, RoutineEffect,
    RoutineEvent, RoutineStartOperationState, WatchState, watch_reconnect_delay_ms,
};

const MAX_FUTURE_DETAIL_RETRIES: u8 = 3;

#[allow(clippy::too_many_lines)]
pub fn reduce(state: &mut ClientState, event: RoutineEvent) -> ClientUpdate {
    match event {
        RoutineEvent::DefinitionWatchInterestRegistered => {
            state.routines.definition_watch.interests =
                state.routines.definition_watch.interests.saturating_add(1);
            maintain_definition_watch(state)
        }
        RoutineEvent::DefinitionWatchInterestReleased => {
            release_watch(&mut state.routines.definition_watch);
            ClientUpdate::default()
        }
        RoutineEvent::DefinitionWatchStartedForOpen { open_id } => {
            open_watch(&mut state.routines.definition_watch, open_id);
            ClientUpdate::default()
        }
        RoutineEvent::DefinitionWatchOpenFailed { open_id, error }
            if state.routines.definition_watch.open_id == Some(open_id) =>
        {
            definition_watch_failed(state, error.to_string())
        }
        RoutineEvent::DefinitionWatchOpenFailed { .. } => ClientUpdate::default(),
        RoutineEvent::DefinitionWatchChanged(snapshot) => {
            if state.routines.definition_digest == snapshot.digest
                && !state.routines.definition_digest.is_empty()
                && state.routines.definitions != snapshot.definitions
            {
                return definition_watch_failed(
                    state,
                    "Routine definition digest repeated with different contents".to_owned(),
                );
            }
            state.routines.definition_digest = snapshot.digest;
            state.routines.definitions = snapshot.definitions;
            receive_watch(&mut state.routines.definition_watch);
            ClientUpdate::default()
        }
        RoutineEvent::DefinitionWatchFailed(error) => {
            definition_watch_failed(state, error.to_string())
        }
        RoutineEvent::DefinitionWatchEnded => {
            definition_watch_failed(state, "Routine definition watch ended".to_owned())
        }
        RoutineEvent::DefinitionReconnectReady => maintain_definition_watch(state),
        RoutineEvent::InvocationWatchInterestRegistered => {
            state.routines.invocation_watch.interests =
                state.routines.invocation_watch.interests.saturating_add(1);
            maintain_invocation_watch(state)
        }
        RoutineEvent::InvocationWatchInterestReleased => {
            release_watch(&mut state.routines.invocation_watch);
            ClientUpdate::default()
        }
        RoutineEvent::InvocationWatchStartedForOpen { open_id } => {
            open_watch(&mut state.routines.invocation_watch, open_id);
            ClientUpdate::default()
        }
        RoutineEvent::InvocationWatchOpenFailed { open_id, error }
            if state.routines.invocation_watch.open_id == Some(open_id) =>
        {
            invocation_watch_failed(state, error.to_string())
        }
        RoutineEvent::InvocationWatchOpenFailed { .. } => ClientUpdate::default(),
        RoutineEvent::InvocationWatchChanged(update) => apply_invocation_update(state, update),
        RoutineEvent::InvocationWatchFailed(error) => {
            invocation_watch_failed(state, error.to_string())
        }
        RoutineEvent::InvocationWatchEnded => {
            invocation_watch_failed(state, "Routine invocation watch ended".to_owned())
        }
        RoutineEvent::InvocationReconnectReady => maintain_invocation_watch(state),
        RoutineEvent::SessionInvocationWatchInterestRegistered { session_id } => {
            let history = state
                .routines
                .session_histories
                .entry(session_id.clone())
                .or_default();
            history.watch.interests = history.watch.interests.saturating_add(1);
            maintain_session_invocation_watch(state, &session_id)
        }
        RoutineEvent::SessionInvocationWatchInterestReleased { session_id } => {
            if let Some(history) = state.routines.session_histories.get_mut(&session_id) {
                history.watch.interests = history.watch.interests.saturating_sub(1);
                if history.watch.interests == 0 {
                    state.routines.session_histories.remove(&session_id);
                }
            }
            ClientUpdate::default()
        }
        RoutineEvent::SessionInvocationWatchStartedForOpen {
            session_id,
            open_id,
        } => {
            if let Some(history) = state.routines.session_histories.get_mut(&session_id) {
                open_watch(&mut history.watch, open_id);
            }
            ClientUpdate::default()
        }
        RoutineEvent::SessionInvocationWatchOpenFailed {
            session_id,
            open_id,
            error,
        } if state
            .routines
            .session_histories
            .get(&session_id)
            .is_some_and(|history| history.watch.open_id == Some(open_id)) =>
        {
            session_invocation_watch_failed(state, &session_id, error.to_string())
        }
        RoutineEvent::SessionInvocationWatchOpenFailed { .. } => ClientUpdate::default(),
        RoutineEvent::SessionInvocationWatchChanged { session_id, update } => {
            apply_session_invocation_update(state, &session_id, update)
        }
        RoutineEvent::SessionInvocationWatchFailed { session_id, error } => {
            session_invocation_watch_failed(state, &session_id, error.to_string())
        }
        RoutineEvent::SessionInvocationWatchEnded { session_id } => {
            session_invocation_watch_failed(
                state,
                &session_id,
                "Routine session invocation watch ended".to_owned(),
            )
        }
        RoutineEvent::SessionInvocationReconnectReady { session_id } => {
            maintain_session_invocation_watch(state, &session_id)
        }
        RoutineEvent::StartRequested {
            operation_key,
            request_id,
            routine_name,
            trigger_name,
            source_session_id,
            inputs,
        } => {
            state.routines.starts.insert(
                operation_key.clone(),
                RoutineStartOperationState {
                    operation_key: operation_key.clone(),
                    request_id: request_id.clone(),
                    routine_name: routine_name.clone(),
                    trigger_name: trigger_name.clone(),
                    source_session_id: source_session_id.clone(),
                    inputs: inputs.clone(),
                    status: OperationStatus::Loading,
                    ..RoutineStartOperationState::default()
                },
            );
            retain_start(state, &operation_key);
            ClientUpdate::with_effect(ClientEffect::Routine(RoutineEffect::StartManual {
                operation_key,
                request_id,
                routine_name,
                trigger_name,
                source_session_id,
                inputs,
            }))
        }
        RoutineEvent::StartAccepted {
            operation_key,
            request_id,
            invocation_id,
        } => {
            set_start_outcome(
                state,
                &operation_key,
                &request_id,
                Ok(ClientRoutineStartOutcome::Accepted(invocation_id)),
            );
            bound_invocations(state);
            ClientUpdate::default()
        }
        RoutineEvent::StartValidationFailed {
            operation_key,
            request_id,
            failure,
        } => {
            set_start_outcome(
                state,
                &operation_key,
                &request_id,
                Ok(ClientRoutineStartOutcome::ValidationFailed(failure)),
            );
            ClientUpdate::default()
        }
        RoutineEvent::StartFailed {
            operation_key,
            request_id,
            error,
        } => {
            set_start_outcome(state, &operation_key, &request_id, Err(error.to_string()));
            ClientUpdate::default()
        }
        RoutineEvent::OlderPageRequested {
            request_id,
            page_size,
            before_admission_sequence,
        } => {
            state.routines.page = crate::RoutinePageState {
                request_id: request_id.clone(),
                before_admission_sequence: Some(before_admission_sequence),
                page_size,
                status: OperationStatus::Loading,
                error: None,
            };
            ClientUpdate::with_effect(ClientEffect::Routine(RoutineEffect::ListInvocations {
                request_id,
                page_size,
                before_admission_sequence,
                metadata_filters: Default::default(),
            }))
        }
        RoutineEvent::OlderPageLoaded {
            request_id,
            before_admission_sequence,
            page,
        } => {
            if page_is_current(state, &request_id, before_admission_sequence) {
                for invocation in page.invocations {
                    state
                        .routines
                        .invocations
                        .insert(invocation.invocation_id.clone(), invocation);
                }
                state.routines.older_page_token = page.older_page_token;
                state.routines.page.status = OperationStatus::Ready;
                state.routines.page.error = None;
                bound_invocations(state);
            }
            ClientUpdate::default()
        }
        RoutineEvent::OlderPageFailed {
            request_id,
            before_admission_sequence,
            error,
        } => {
            if page_is_current(state, &request_id, before_admission_sequence) {
                state.routines.page.status = OperationStatus::Failed;
                state.routines.page.error = Some(error.to_string());
            }
            ClientUpdate::default()
        }
        RoutineEvent::DetailRequested {
            request_id,
            invocation_id,
        } => {
            let existing = state
                .routines
                .details
                .get(&invocation_id)
                .and_then(|detail| detail.detail.clone());
            state.routines.details.insert(
                invocation_id.clone(),
                crate::RoutineDetailState {
                    request_id: request_id.clone(),
                    status: OperationStatus::Loading,
                    detail: existing,
                    error: None,
                    future_retry_count: 0,
                },
            );
            retain_detail(state, &invocation_id);
            ClientUpdate::with_effect(ClientEffect::Routine(RoutineEffect::GetInvocation {
                request_id,
                invocation_id,
            }))
        }
        RoutineEvent::DetailLoaded {
            request_id,
            invocation_id,
            detail,
        } => {
            if detail_is_current(state, &invocation_id, &request_id) {
                if detail.summary.invocation_id != invocation_id {
                    return ClientUpdate::default();
                }
                if state
                    .routines
                    .invocations
                    .get(&invocation_id)
                    .is_some_and(|summary| !detail_is_consistent_with_summary(&detail, summary))
                {
                    return ClientUpdate::with_effect(ClientEffect::Routine(
                        RoutineEffect::GetInvocation {
                            request_id,
                            invocation_id,
                        },
                    ));
                }
                if !state.routines.invocations.contains_key(&invocation_id)
                    && state
                        .routines
                        .through_sequence
                        .is_some_and(|cursor| detail.summary.latest_sequence > cursor)
                {
                    return retain_future_detail_or_retry(state, request_id, invocation_id, detail);
                }
                state.routines.details.insert(
                    invocation_id,
                    crate::RoutineDetailState {
                        request_id,
                        status: OperationStatus::Ready,
                        detail: Some(detail),
                        error: None,
                        future_retry_count: 0,
                    },
                );
                bound_invocations(state);
            }
            ClientUpdate::default()
        }
        RoutineEvent::DetailFailed {
            request_id,
            invocation_id,
            error,
        } => {
            if let Some(detail) = state.routines.details.get_mut(&invocation_id)
                && detail.request_id == request_id
                && detail.status == OperationStatus::Loading
            {
                detail.status = OperationStatus::Failed;
                detail.error = Some(error.to_string());
            }
            ClientUpdate::default()
        }
    }
}

pub fn maintain_definition_watch(state: &mut ClientState) -> ClientUpdate {
    if !watch_needs_open(&state.routines.definition_watch) {
        return ClientUpdate::default();
    }
    if !state.permits_watch_maintenance() {
        state.routines.definition_watch.status = OperationStatus::Waiting;
        return ClientUpdate::default();
    }
    let Some(open_id) = state.next_watch_open_id() else {
        state.routines.definition_watch.status = OperationStatus::Failed;
        state.routines.definition_watch.error = Some("watch open identity exhausted".to_owned());
        return ClientUpdate::default();
    };
    start_watch(&mut state.routines.definition_watch, open_id);
    ClientUpdate::with_effect(ClientEffect::Routine(RoutineEffect::WatchDefinitions {
        open_id,
        request_id: format!("routine-definitions-{open_id}"),
    }))
}

pub fn maintain_invocation_watch(state: &mut ClientState) -> ClientUpdate {
    if !watch_needs_open(&state.routines.invocation_watch) {
        return ClientUpdate::default();
    }
    if !state.permits_watch_maintenance() {
        state.routines.invocation_watch.status = OperationStatus::Waiting;
        return ClientUpdate::default();
    }
    let Some(open_id) = state.next_watch_open_id() else {
        state.routines.invocation_watch.status = OperationStatus::Failed;
        state.routines.invocation_watch.error = Some("watch open identity exhausted".to_owned());
        return ClientUpdate::default();
    };
    let after_sequence = (!state.routines.force_invocation_snapshot)
        .then_some(state.routines.through_sequence)
        .flatten();
    state.routines.force_invocation_snapshot = false;
    start_watch(&mut state.routines.invocation_watch, open_id);
    ClientUpdate::with_effect(ClientEffect::Routine(RoutineEffect::WatchInvocations {
        open_id,
        request_id: format!("routine-invocations-{open_id}"),
        after_sequence,
        metadata_filters: Default::default(),
        session_id: None,
    }))
}

pub fn maintain_session_invocation_watch(
    state: &mut ClientState,
    session_id: &str,
) -> ClientUpdate {
    let Some(history) = state.routines.session_histories.get(session_id) else {
        return ClientUpdate::default();
    };
    if !watch_needs_open(&history.watch) {
        return ClientUpdate::default();
    }
    if !state.permits_watch_maintenance() {
        state
            .routines
            .session_histories
            .get_mut(session_id)
            .unwrap()
            .watch
            .status = OperationStatus::Waiting;
        return ClientUpdate::default();
    }
    let Some(open_id) = state.next_watch_open_id() else {
        let watch = &mut state
            .routines
            .session_histories
            .get_mut(session_id)
            .unwrap()
            .watch;
        watch.status = OperationStatus::Failed;
        watch.error = Some("watch open identity exhausted".to_owned());
        return ClientUpdate::default();
    };
    let history = state
        .routines
        .session_histories
        .get_mut(session_id)
        .unwrap();
    let after_sequence = (!history.force_snapshot)
        .then_some(history.through_sequence)
        .flatten();
    history.force_snapshot = false;
    start_watch(&mut history.watch, open_id);
    ClientUpdate::with_effect(ClientEffect::Routine(RoutineEffect::WatchInvocations {
        open_id,
        request_id: format!("routine-session-{session_id}-{open_id}"),
        after_sequence,
        metadata_filters: std::collections::BTreeMap::from([(
            crate::ROUTINE_SOURCE_SESSION_ID_METADATA_KEY.to_owned(),
            session_id.to_owned(),
        )]),
        session_id: Some(session_id.to_owned()),
    }))
}

fn apply_session_invocation_update(
    state: &mut ClientState,
    session_id: &str,
    update: ClientRoutineInvocationWatchUpdate,
) -> ClientUpdate {
    let Some(history) = state.routines.session_histories.get(session_id).cloned() else {
        return ClientUpdate::default();
    };
    match update {
        ClientRoutineInvocationWatchUpdate::Snapshot(snapshot) => {
            if snapshot
                .invocations
                .iter()
                .any(|invocation| invocation.latest_sequence > snapshot.through_sequence)
            {
                return recover_session_invocation_snapshot(
                    state,
                    session_id,
                    "Routine session snapshot cursor precedes a contained invocation".to_owned(),
                );
            }
            let history = state
                .routines
                .session_histories
                .get_mut(session_id)
                .unwrap();
            history.invocations = snapshot
                .invocations
                .into_iter()
                .map(|summary| (summary.invocation_id.clone(), summary))
                .collect();
            history.through_sequence = Some(snapshot.through_sequence);
            receive_watch(&mut history.watch);
            bound_session_invocations(history);
            ClientUpdate::default()
        }
        ClientRoutineInvocationWatchUpdate::Event(event) => {
            let Some(cursor) = history.through_sequence else {
                return recover_session_invocation_snapshot(
                    state,
                    session_id,
                    "Routine session replay arrived before an authoritative cursor".to_owned(),
                );
            };
            if event.sequence <= cursor {
                return recover_session_invocation_snapshot(
                    state,
                    session_id,
                    "Routine session replay sequence did not increase".to_owned(),
                );
            }
            let mut replay = crate::RoutineClientState {
                invocations: history.invocations,
                through_sequence: history.through_sequence,
                ..crate::RoutineClientState::default()
            };
            if let Err(message) = reduce_invocation_event(&mut replay, &event) {
                return recover_session_invocation_snapshot(state, session_id, message);
            }
            let history = state
                .routines
                .session_histories
                .get_mut(session_id)
                .unwrap();
            history.invocations = replay.invocations;
            history.through_sequence = Some(event.sequence);
            receive_watch(&mut history.watch);
            bound_session_invocations(history);
            ClientUpdate::default()
        }
    }
}

fn bound_session_invocations(history: &mut crate::SessionRoutineHistoryState) {
    let mut ids = history
        .invocations
        .values()
        .map(|summary| {
            (
                summary.admission_sequence,
                summary.latest_sequence,
                summary.invocation_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    ids.sort_by(|left, right| right.cmp(left));
    for (_, _, id) in ids
        .into_iter()
        .skip(crate::ROUTINE_INVOCATION_SNAPSHOT_SIZE as usize)
    {
        history.invocations.remove(&id);
    }
}

fn recover_session_invocation_snapshot(
    state: &mut ClientState,
    session_id: &str,
    message: String,
) -> ClientUpdate {
    let Some(history) = state.routines.session_histories.get_mut(session_id) else {
        return ClientUpdate::default();
    };
    history.through_sequence = None;
    history.force_snapshot = true;
    history.watch.status = OperationStatus::Waiting;
    history.watch.error = Some(message);
    history.watch.open_id = None;
    maintain_session_invocation_watch(state, session_id)
}

fn apply_invocation_update(
    state: &mut ClientState,
    update: ClientRoutineInvocationWatchUpdate,
) -> ClientUpdate {
    match update {
        ClientRoutineInvocationWatchUpdate::Snapshot(snapshot) => {
            if snapshot
                .invocations
                .iter()
                .any(|invocation| invocation.latest_sequence > snapshot.through_sequence)
            {
                return recover_invocation_snapshot(
                    state,
                    "Routine snapshot cursor precedes a contained invocation".to_owned(),
                );
            }
            let pinned = pinned_invocation_ids(&state.routines);
            let pinned_summaries = state
                .routines
                .invocations
                .iter()
                .filter(|(id, _)| pinned.contains(*id))
                .map(|(id, summary)| (id.clone(), summary.clone()))
                .collect::<Vec<_>>();
            state.routines.invocations = snapshot
                .invocations
                .into_iter()
                .map(|invocation| (invocation.invocation_id.clone(), invocation))
                .collect();
            for (id, summary) in pinned_summaries {
                state.routines.invocations.entry(id).or_insert(summary);
            }
            for (invocation_id, detail_state) in &mut state.routines.details {
                if let Some(detail) = detail_state.detail.as_mut()
                    && let Some(summary) = state.routines.invocations.get(invocation_id)
                    && detail.summary.latest_sequence <= summary.latest_sequence
                {
                    detail.summary.clone_from(summary);
                }
            }
            state.routines.through_sequence = Some(snapshot.through_sequence);
            state.routines.older_page_token = snapshot.older_page_token;
            state.routines.page = crate::RoutinePageState::default();
            receive_watch(&mut state.routines.invocation_watch);
            bound_invocations(state);
            ClientUpdate::default()
        }
        ClientRoutineInvocationWatchUpdate::Event(event) => {
            let Some(cursor) = state.routines.through_sequence else {
                return recover_invocation_snapshot(
                    state,
                    "Routine replay event arrived before an authoritative cursor".to_owned(),
                );
            };
            if event.sequence <= cursor {
                return recover_invocation_snapshot(
                    state,
                    "Routine replay sequence did not increase".to_owned(),
                );
            }
            let mut routines = state.routines.clone();
            if let Err(message) = reduce_invocation_event(&mut routines, &event) {
                return recover_invocation_snapshot(state, message);
            }
            routines.through_sequence = Some(event.sequence);
            receive_watch(&mut routines.invocation_watch);
            state.routines = routines;
            bound_invocations(state);
            ClientUpdate::default()
        }
    }
}

#[allow(clippy::too_many_lines)]
fn reduce_invocation_event(
    routines: &mut crate::RoutineClientState,
    event: &ClientRoutineInvocationEvent,
) -> Result<(), String> {
    validate_replay_event_shape(event)?;
    if event.kind == ClientRoutineEventKind::InvocationAdmitted {
        if routines.invocations.contains_key(&event.invocation_id) {
            return Err("Routine admission replay duplicated an invocation".to_owned());
        }
        let admission = event
            .admission
            .as_ref()
            .ok_or_else(|| "Routine admission replay omitted metadata".to_owned())?;
        if admission.admission_sequence == 0 {
            return Err("Routine admission sequence is zero".to_owned());
        }
        let summary = ClientRoutineInvocationSummary {
            admission_sequence: admission.admission_sequence,
            invocation_id: event.invocation_id.clone(),
            routine_name: admission.routine_name.clone(),
            routine_revision: admission.routine_revision.clone(),
            trigger_name: admission.trigger_name.clone(),
            trigger_kind: admission.trigger_kind,
            correlation_id: admission.correlation_id.clone(),
            metadata: admission.metadata.clone(),
            source_cursor: admission.source_cursor.clone(),
            status: ClientRoutineInvocationStatus::Admitted,
            admitted_at_ms: admission.admitted_at_ms,
            started_at_ms: None,
            completed_at_ms: None,
            updated_at_ms: event.occurred_at_ms,
            terminal_error: None,
            latest_sequence: event.sequence,
        };
        let detail = ClientRoutineInvocationDetail {
            summary: summary.clone(),
            tasks: admission
                .declared_tasks
                .iter()
                .map(|task| ClientRoutineTask {
                    name: task.name.clone(),
                    r#type: task.r#type.clone(),
                    status: ClientRoutineTaskStatus::Pending,
                    results: Vec::new(),
                    session_references: Default::default(),
                    error: None,
                })
                .collect(),
            results: Vec::new(),
        };
        routines
            .invocations
            .insert(event.invocation_id.clone(), summary);
        routines.details.insert(
            event.invocation_id.clone(),
            crate::RoutineDetailState {
                status: OperationStatus::Ready,
                detail: Some(detail),
                ..crate::RoutineDetailState::default()
            },
        );
        return Ok(());
    }

    let current_status = routines
        .invocations
        .get(&event.invocation_id)
        .map(|summary| summary.status)
        .ok_or_else(|| "Routine replay referenced an unknown invocation".to_owned())?;
    let detail_covers_event = routines
        .details
        .get(&event.invocation_id)
        .and_then(|state| state.detail.as_ref())
        .is_some_and(|detail| detail.summary.latest_sequence >= event.sequence);
    if current_status.is_terminal() {
        let repeated = matches!(
            (current_status, event.kind),
            (
                ClientRoutineInvocationStatus::Succeeded,
                ClientRoutineEventKind::InvocationSucceeded
            ) | (
                ClientRoutineInvocationStatus::Failed,
                ClientRoutineEventKind::InvocationFailed
            ) | (
                ClientRoutineInvocationStatus::Interrupted,
                ClientRoutineEventKind::InvocationInterrupted
            )
        );
        if !repeated {
            return Err("Routine replay attempted to regress a terminal invocation".to_owned());
        }
    }
    match event.kind {
        ClientRoutineEventKind::InvocationStarted => {
            if current_status != ClientRoutineInvocationStatus::Admitted {
                return Err("Routine start replay has an illegal predecessor".to_owned());
            }
            let summary = routines
                .invocations
                .get_mut(&event.invocation_id)
                .expect("summary existence was checked");
            summary.status = ClientRoutineInvocationStatus::Running;
            summary.started_at_ms = Some(event.occurred_at_ms);
        }
        ClientRoutineEventKind::TaskStarted
        | ClientRoutineEventKind::TaskSucceeded
        | ClientRoutineEventKind::TaskSkipped
        | ClientRoutineEventKind::TaskBlocked
        | ClientRoutineEventKind::TaskFailed
        | ClientRoutineEventKind::TaskInterrupted => {
            let interruption_before_start = event.kind == ClientRoutineEventKind::TaskInterrupted
                && current_status == ClientRoutineInvocationStatus::Admitted;
            if current_status != ClientRoutineInvocationStatus::Running
                && !interruption_before_start
            {
                return Err("Routine task replay occurred outside a running invocation".to_owned());
            }
            if routines
                .details
                .get(&event.invocation_id)
                .and_then(|state| state.detail.as_ref())
                .is_some()
            {
                if detail_covers_event {
                    validate_task_is_declared(routines, event)?;
                } else {
                    reduce_task_event(routines, event)?;
                }
            }
        }
        ClientRoutineEventKind::InvocationSucceeded
        | ClientRoutineEventKind::InvocationFailed
        | ClientRoutineEventKind::InvocationInterrupted => {
            if !matches!(
                current_status,
                ClientRoutineInvocationStatus::Admitted | ClientRoutineInvocationStatus::Running
            ) && !current_status.is_terminal()
            {
                return Err("Routine terminal replay has an illegal predecessor".to_owned());
            }
            let summary = routines
                .invocations
                .get_mut(&event.invocation_id)
                .expect("summary existence was checked");
            summary.status = match event.kind {
                ClientRoutineEventKind::InvocationSucceeded => {
                    ClientRoutineInvocationStatus::Succeeded
                }
                ClientRoutineEventKind::InvocationFailed => ClientRoutineInvocationStatus::Failed,
                _ => ClientRoutineInvocationStatus::Interrupted,
            };
            summary.completed_at_ms = Some(event.occurred_at_ms);
            summary.terminal_error = event.error.clone();
            if !detail_covers_event
                && let Some(detail) = routines
                    .details
                    .get_mut(&event.invocation_id)
                    .and_then(|state| state.detail.as_mut())
            {
                detail.results.clone_from(&event.values);
            }
        }
        ClientRoutineEventKind::InvocationAdmitted => unreachable!(),
    }
    let summary = routines
        .invocations
        .get_mut(&event.invocation_id)
        .expect("summary existence was checked");
    summary.updated_at_ms = event.occurred_at_ms;
    summary.latest_sequence = event.sequence;
    if !detail_covers_event
        && let Some(detail) = routines
            .details
            .get_mut(&event.invocation_id)
            .and_then(|state| state.detail.as_mut())
    {
        detail.summary.clone_from(summary);
    }
    Ok(())
}

fn validate_task_is_declared(
    routines: &crate::RoutineClientState,
    event: &ClientRoutineInvocationEvent,
) -> Result<(), String> {
    let task_name = event
        .task_name
        .as_deref()
        .ok_or_else(|| "Routine task replay omitted its task name".to_owned())?;
    if routines
        .details
        .get(&event.invocation_id)
        .and_then(|state| state.detail.as_ref())
        .is_some_and(|detail| detail.tasks.iter().any(|task| task.name == task_name))
    {
        Ok(())
    } else {
        Err("Routine task replay referenced an undeclared task".to_owned())
    }
}

fn validate_replay_event_shape(event: &ClientRoutineInvocationEvent) -> Result<(), String> {
    if event.sequence == 0 || event.invocation_id.is_empty() {
        return Err("Routine replay event has an empty identity or cursor".to_owned());
    }
    let is_admission = event.kind == ClientRoutineEventKind::InvocationAdmitted;
    if is_admission != event.admission.is_some() {
        return Err("Routine replay event has an invalid admission shape".to_owned());
    }
    let is_task = matches!(
        event.kind,
        ClientRoutineEventKind::TaskStarted
            | ClientRoutineEventKind::TaskSucceeded
            | ClientRoutineEventKind::TaskSkipped
            | ClientRoutineEventKind::TaskBlocked
            | ClientRoutineEventKind::TaskFailed
            | ClientRoutineEventKind::TaskInterrupted
    );
    if is_task
        != event
            .task_name
            .as_ref()
            .is_some_and(|name| !name.is_empty())
    {
        return Err("Routine replay event has an invalid task shape".to_owned());
    }
    let error_allowed = matches!(
        event.kind,
        ClientRoutineEventKind::TaskBlocked
            | ClientRoutineEventKind::TaskFailed
            | ClientRoutineEventKind::TaskInterrupted
            | ClientRoutineEventKind::InvocationFailed
            | ClientRoutineEventKind::InvocationInterrupted
    );
    if event.error.is_some() && !error_allowed {
        return Err("Routine replay event has an unexpected error".to_owned());
    }
    let values_allowed = matches!(
        event.kind,
        ClientRoutineEventKind::TaskSucceeded | ClientRoutineEventKind::InvocationSucceeded
    );
    if (!event.values.is_empty() && !values_allowed)
        || (!event.session_references.is_empty()
            && event.kind != ClientRoutineEventKind::TaskSucceeded)
    {
        return Err("Routine replay event has unexpected result data".to_owned());
    }
    Ok(())
}

fn reduce_task_event(
    routines: &mut crate::RoutineClientState,
    event: &ClientRoutineInvocationEvent,
) -> Result<(), String> {
    let task_name = event
        .task_name
        .as_deref()
        .ok_or_else(|| "Routine task replay omitted its task name".to_owned())?;
    let task = routines
        .details
        .get_mut(&event.invocation_id)
        .and_then(|state| state.detail.as_mut())
        .and_then(|detail| detail.tasks.iter_mut().find(|task| task.name == task_name))
        .ok_or_else(|| "Routine task replay referenced an undeclared task".to_owned())?;
    let next = match event.kind {
        ClientRoutineEventKind::TaskStarted => ClientRoutineTaskStatus::Running,
        ClientRoutineEventKind::TaskSucceeded => ClientRoutineTaskStatus::Succeeded,
        ClientRoutineEventKind::TaskSkipped => ClientRoutineTaskStatus::Skipped,
        ClientRoutineEventKind::TaskBlocked => ClientRoutineTaskStatus::Blocked,
        ClientRoutineEventKind::TaskFailed => ClientRoutineTaskStatus::Failed,
        ClientRoutineEventKind::TaskInterrupted => ClientRoutineTaskStatus::Interrupted,
        _ => unreachable!(),
    };
    if task.status.is_terminal() {
        if task.status == next {
            return Ok(());
        }
        return Err("Routine task replay attempted to regress a terminal task".to_owned());
    }
    if (next == ClientRoutineTaskStatus::Running && task.status != ClientRoutineTaskStatus::Pending)
        || (next.is_terminal()
            && !matches!(
                task.status,
                ClientRoutineTaskStatus::Pending | ClientRoutineTaskStatus::Running
            ))
    {
        return Err("Routine task replay has an illegal predecessor".to_owned());
    }
    task.status = next;
    task.results.clone_from(&event.values);
    task.session_references
        .clone_from(&event.session_references);
    task.error.clone_from(&event.error);
    Ok(())
}

fn recover_invocation_snapshot(state: &mut ClientState, message: String) -> ClientUpdate {
    state.routines.through_sequence = None;
    state.routines.force_invocation_snapshot = true;
    state.routines.invocation_watch.status = OperationStatus::Waiting;
    state.routines.invocation_watch.error = Some(message);
    state.routines.invocation_watch.open_id = None;
    maintain_invocation_watch(state)
}

fn definition_watch_failed(state: &mut ClientState, message: String) -> ClientUpdate {
    watch_failed(state, message, true)
}

fn invocation_watch_failed(state: &mut ClientState, message: String) -> ClientUpdate {
    watch_failed(state, message, false)
}

fn session_invocation_watch_failed(
    state: &mut ClientState,
    session_id: &str,
    message: String,
) -> ClientUpdate {
    let permits_maintenance = state.permits_watch_maintenance();
    let Some(history) = state.routines.session_histories.get_mut(session_id) else {
        return ClientUpdate::default();
    };
    if history.watch.interests == 0 {
        return ClientUpdate::default();
    }
    history.watch.status = OperationStatus::Failed;
    history.watch.error = Some(message);
    history.watch.open_id = None;
    let delay_ms = watch_reconnect_delay_ms(history.watch.retry_attempt);
    history.watch.retry_attempt = history.watch.retry_attempt.saturating_add(1);
    if permits_maintenance {
        ClientUpdate::with_effect(ClientEffect::Routine(
            RoutineEffect::SleepSessionInvocationReconnect {
                session_id: session_id.to_owned(),
                delay_ms,
            },
        ))
    } else {
        history.watch.status = OperationStatus::Waiting;
        ClientUpdate::default()
    }
}

fn watch_failed(state: &mut ClientState, message: String, definition: bool) -> ClientUpdate {
    let permits_maintenance = state.permits_watch_maintenance();
    let watch = if definition {
        &mut state.routines.definition_watch
    } else {
        &mut state.routines.invocation_watch
    };
    if watch.interests == 0 {
        return ClientUpdate::default();
    }
    watch.status = OperationStatus::Failed;
    watch.error = Some(message);
    watch.open_id = None;
    let delay_ms = watch_reconnect_delay_ms(watch.retry_attempt);
    watch.retry_attempt = watch.retry_attempt.saturating_add(1);
    if permits_maintenance {
        let effect = if definition {
            RoutineEffect::SleepDefinitionReconnect { delay_ms }
        } else {
            RoutineEffect::SleepInvocationReconnect { delay_ms }
        };
        ClientUpdate::with_effect(ClientEffect::Routine(effect))
    } else {
        watch.status = OperationStatus::Waiting;
        ClientUpdate::default()
    }
}

fn watch_needs_open(watch: &WatchState) -> bool {
    watch.interests > 0
        && !matches!(
            watch.status,
            OperationStatus::Loading | OperationStatus::Opened | OperationStatus::Received
        )
}

fn start_watch(watch: &mut WatchState, open_id: u64) {
    watch.status = OperationStatus::Loading;
    watch.error = None;
    watch.open_id = Some(open_id);
}

fn open_watch(watch: &mut WatchState, open_id: u64) {
    if watch.open_id == Some(open_id) {
        watch.status = OperationStatus::Opened;
        watch.error = None;
        watch.open_id = None;
    }
}

fn receive_watch(watch: &mut WatchState) {
    watch.status = OperationStatus::Received;
    watch.error = None;
    watch.retry_attempt = 0;
}

fn release_watch(watch: &mut WatchState) {
    watch.interests = watch.interests.saturating_sub(1);
    if watch.interests == 0 {
        *watch = WatchState::default();
    }
}

fn set_start_outcome(
    state: &mut ClientState,
    operation_key: &str,
    request_id: &str,
    outcome: Result<ClientRoutineStartOutcome, String>,
) {
    let Some(start) = state.routines.starts.get_mut(operation_key) else {
        return;
    };
    if start.request_id != request_id || start.status != OperationStatus::Loading {
        return;
    }
    match outcome {
        Ok(ClientRoutineStartOutcome::Accepted(invocation_id)) => {
            start.status = OperationStatus::Ready;
            start.invocation_id = Some(invocation_id);
            start.validation_failure = None;
            start.error = None;
        }
        Ok(ClientRoutineStartOutcome::ValidationFailed(failure)) => {
            start.status = OperationStatus::Failed;
            start.invocation_id = None;
            start.validation_failure = Some(failure);
            start.error = None;
        }
        Err(error) => {
            start.status = OperationStatus::Failed;
            start.invocation_id = None;
            start.validation_failure = None;
            start.error = Some(error);
        }
    }
}

fn retain_start(state: &mut ClientState, key: &str) {
    state
        .routines
        .start_order
        .retain(|existing| existing != key);
    state.routines.start_order.push_back(key.to_owned());
    while state.routines.start_order.len() > MAX_RETAINED_ROUTINE_STARTS {
        if let Some(oldest) = state.routines.start_order.pop_front() {
            state.routines.starts.remove(&oldest);
        }
    }
    bound_invocations(state);
}

fn retain_detail(state: &mut ClientState, invocation_id: &str) {
    state
        .routines
        .detail_order
        .retain(|existing| existing != invocation_id);
    state
        .routines
        .detail_order
        .push_back(invocation_id.to_owned());
    while state.routines.detail_order.len() > MAX_RETAINED_ROUTINE_DETAILS {
        if let Some(oldest) = state.routines.detail_order.pop_front() {
            state.routines.details.remove(&oldest);
        }
    }
    bound_invocations(state);
}

fn bound_invocations(state: &mut ClientState) {
    let pinned = pinned_invocation_ids(&state.routines);
    let mut ids = state
        .routines
        .invocations
        .values()
        .filter(|summary| !pinned.contains(&summary.invocation_id))
        .map(|summary| {
            (
                summary.admission_sequence,
                summary.latest_sequence,
                summary.invocation_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    ids.sort_by(|left, right| right.cmp(left));
    if ids.len() > MAX_RETAINED_ROUTINE_INVOCATIONS {
        let retained_boundary = ids[MAX_RETAINED_ROUTINE_INVOCATIONS - 1].0;
        state.routines.older_page_token = Some(
            state
                .routines
                .older_page_token
                .map_or(retained_boundary, |known| known.max(retained_boundary)),
        );
        for (_, _, id) in ids.into_iter().skip(MAX_RETAINED_ROUTINE_INVOCATIONS) {
            state.routines.invocations.remove(&id);
        }
    }
    state.routines.details.retain(|id, _| {
        state.routines.invocations.contains_key(id)
            || state
                .routines
                .detail_order
                .iter()
                .any(|loaded| loaded == id)
    });
}

fn pinned_invocation_ids(routines: &crate::RoutineClientState) -> BTreeSet<String> {
    routines
        .starts
        .values()
        .filter_map(|start| start.invocation_id.clone())
        .chain(routines.detail_order.iter().cloned())
        .collect()
}

fn page_is_current(state: &ClientState, request_id: &str, before: u64) -> bool {
    state.routines.page.status == OperationStatus::Loading
        && state.routines.page.request_id == request_id
        && state.routines.page.before_admission_sequence == Some(before)
}

fn detail_is_current(state: &ClientState, invocation_id: &str, request_id: &str) -> bool {
    state
        .routines
        .details
        .get(invocation_id)
        .is_some_and(|detail| {
            detail.status == OperationStatus::Loading && detail.request_id == request_id
        })
}

fn retain_future_detail_or_retry(
    state: &mut ClientState,
    request_id: String,
    invocation_id: String,
    detail: ClientRoutineInvocationDetail,
) -> ClientUpdate {
    let detail_state = state
        .routines
        .details
        .get_mut(&invocation_id)
        .expect("current detail operation exists");
    detail_state.detail = Some(detail);
    detail_state.future_retry_count = detail_state.future_retry_count.saturating_add(1);
    if detail_state.future_retry_count >= MAX_FUTURE_DETAIL_RETRIES {
        detail_state.status = OperationStatus::Failed;
        detail_state.error = Some(
            "Routine detail is newer than history and its invocation is not retained".to_owned(),
        );
        return ClientUpdate::default();
    }
    ClientUpdate::with_effect(ClientEffect::Routine(RoutineEffect::GetInvocation {
        request_id,
        invocation_id,
    }))
}

fn detail_is_consistent_with_summary(
    detail: &ClientRoutineInvocationDetail,
    current: &ClientRoutineInvocationSummary,
) -> bool {
    let loaded = &detail.summary;
    loaded.latest_sequence >= current.latest_sequence
        && loaded.invocation_id == current.invocation_id
        && loaded.admission_sequence == current.admission_sequence
        && loaded.routine_name == current.routine_name
        && loaded.routine_revision == current.routine_revision
        && loaded.trigger_name == current.trigger_name
        && loaded.trigger_kind == current.trigger_kind
        && loaded.correlation_id == current.correlation_id
        && loaded.metadata == current.metadata
        && loaded.source_cursor == current.source_cursor
        && (loaded.latest_sequence != current.latest_sequence || loaded == current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AccessStatus, Client, ClientEvent, ClientManualRoutineDefinition, ClientRoutineAdmission,
        ClientRoutineDefinitionSnapshot, ClientRoutineInvocationSnapshot,
        ClientRoutineTaskDescriptor, ClientRoutineTriggerKind,
    };

    fn admitted(sequence: u64, id: &str) -> ClientRoutineInvocationEvent {
        admitted_with_key(sequence, sequence, id)
    }

    fn admitted_with_key(
        sequence: u64,
        admission_sequence: u64,
        id: &str,
    ) -> ClientRoutineInvocationEvent {
        ClientRoutineInvocationEvent {
            sequence,
            invocation_id: id.to_owned(),
            kind: ClientRoutineEventKind::InvocationAdmitted,
            task_name: None,
            occurred_at_ms: 10,
            error: None,
            values: Vec::new(),
            session_references: Default::default(),
            admission: Some(ClientRoutineAdmission {
                admission_sequence,
                routine_name: "fork".to_owned(),
                routine_revision: "revision-1".to_owned(),
                trigger_name: "manual".to_owned(),
                trigger_kind: ClientRoutineTriggerKind::Manual,
                correlation_id: "request".to_owned(),
                metadata: std::collections::BTreeMap::from([(
                    crate::ROUTINE_SOURCE_SESSION_ID_METADATA_KEY.to_owned(),
                    "source".to_owned(),
                )]),
                source_cursor: None,
                admitted_at_ms: 10,
                declared_tasks: vec![ClientRoutineTaskDescriptor {
                    name: "create".to_owned(),
                    r#type: "sessionCreate".to_owned(),
                }],
            }),
        }
    }

    fn event(
        sequence: u64,
        id: &str,
        kind: ClientRoutineEventKind,
        task_name: Option<&str>,
    ) -> ClientRoutineInvocationEvent {
        ClientRoutineInvocationEvent {
            sequence,
            invocation_id: id.to_owned(),
            kind,
            task_name: task_name.map(str::to_owned),
            occurred_at_ms: i64::try_from(sequence).unwrap(),
            error: matches!(
                kind,
                ClientRoutineEventKind::TaskBlocked
                    | ClientRoutineEventKind::TaskFailed
                    | ClientRoutineEventKind::TaskInterrupted
                    | ClientRoutineEventKind::InvocationFailed
                    | ClientRoutineEventKind::InvocationInterrupted
            )
            .then(|| crate::ClientRoutineFailure {
                code: "failed".to_owned(),
                message: "safe failure".to_owned(),
            }),
            values: Vec::new(),
            session_references: Default::default(),
            admission: None,
        }
    }

    fn replay_client() -> Client {
        let mut client = Client::new();
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Snapshot(ClientRoutineInvocationSnapshot {
                invocations: Vec::new(),
                through_sequence: 1,
                older_page_token: None,
            }),
        )));
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Event(admitted(2, "invocation")),
        )));
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Event(event(
                3,
                "invocation",
                ClientRoutineEventKind::InvocationStarted,
                None,
            )),
        )));
        client
    }

    fn summary(
        id: &str,
        admission_sequence: u64,
        latest_sequence: u64,
        status: ClientRoutineInvocationStatus,
    ) -> ClientRoutineInvocationSummary {
        ClientRoutineInvocationSummary {
            admission_sequence,
            invocation_id: id.to_owned(),
            routine_name: "fork".to_owned(),
            routine_revision: "revision-1".to_owned(),
            trigger_name: "manual".to_owned(),
            trigger_kind: ClientRoutineTriggerKind::Manual,
            correlation_id: format!("correlation-{id}"),
            metadata: std::collections::BTreeMap::from([(
                crate::ROUTINE_SOURCE_SESSION_ID_METADATA_KEY.to_owned(),
                "source".to_owned(),
            )]),
            source_cursor: None,
            status,
            admitted_at_ms: 1,
            started_at_ms: (status != ClientRoutineInvocationStatus::Admitted).then_some(2),
            completed_at_ms: status.is_terminal().then_some(3),
            updated_at_ms: i64::try_from(latest_sequence).unwrap(),
            terminal_error: None,
            latest_sequence,
        }
    }

    fn detail(summary: ClientRoutineInvocationSummary) -> ClientRoutineInvocationDetail {
        ClientRoutineInvocationDetail {
            summary,
            tasks: vec![ClientRoutineTask {
                name: "create".to_owned(),
                r#type: "sessionCreate".to_owned(),
                status: ClientRoutineTaskStatus::Pending,
                results: Vec::new(),
                session_references: Default::default(),
                error: None,
            }],
            results: Vec::new(),
        }
    }

    #[test]
    fn complete_definition_snapshots_replace_global_order_without_touching_pinned_detail() {
        let mut client = replay_client();
        let definition = |name: &str, revision: &str| ClientManualRoutineDefinition {
            routine_name: name.to_owned(),
            trigger_name: "manual".to_owned(),
            display_name: name.to_owned(),
            description: String::new(),
            scope: crate::ClientRoutineScope::Session,
            inputs: Vec::new(),
            results: Vec::new(),
            routine_revision: revision.to_owned(),
        };
        client.apply(ClientEvent::Routine(RoutineEvent::DefinitionWatchChanged(
            ClientRoutineDefinitionSnapshot {
                digest: "one".to_owned(),
                definitions: vec![definition("old", "live-1")],
            },
        )));
        client.apply(ClientEvent::Routine(RoutineEvent::DefinitionWatchChanged(
            ClientRoutineDefinitionSnapshot {
                digest: "two".to_owned(),
                definitions: vec![definition("new", "live-2")],
            },
        )));

        assert_eq!(client.state().routines.definitions[0].routine_name, "new");
        let detail = client.state().routines.details["invocation"]
            .detail
            .as_ref()
            .unwrap();
        assert_eq!(detail.summary.routine_revision, "revision-1");
    }

    #[test]
    fn every_task_terminal_state_and_invocation_terminal_state_reduces() {
        let task_cases = [
            (
                ClientRoutineEventKind::TaskSucceeded,
                ClientRoutineTaskStatus::Succeeded,
            ),
            (
                ClientRoutineEventKind::TaskSkipped,
                ClientRoutineTaskStatus::Skipped,
            ),
            (
                ClientRoutineEventKind::TaskBlocked,
                ClientRoutineTaskStatus::Blocked,
            ),
            (
                ClientRoutineEventKind::TaskFailed,
                ClientRoutineTaskStatus::Failed,
            ),
            (
                ClientRoutineEventKind::TaskInterrupted,
                ClientRoutineTaskStatus::Interrupted,
            ),
        ];
        for (kind, expected) in task_cases {
            let mut client = replay_client();
            client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
                ClientRoutineInvocationWatchUpdate::Event(event(
                    4,
                    "invocation",
                    ClientRoutineEventKind::TaskStarted,
                    Some("create"),
                )),
            )));
            assert_eq!(
                client.state().routines.details["invocation"]
                    .detail
                    .as_ref()
                    .unwrap()
                    .tasks[0]
                    .status,
                ClientRoutineTaskStatus::Running
            );
            client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
                ClientRoutineInvocationWatchUpdate::Event(event(
                    5,
                    "invocation",
                    kind,
                    Some("create"),
                )),
            )));
            assert_eq!(
                client.state().routines.details["invocation"]
                    .detail
                    .as_ref()
                    .unwrap()
                    .tasks[0]
                    .status,
                expected
            );
        }

        for (kind, expected) in [
            (
                ClientRoutineEventKind::InvocationSucceeded,
                ClientRoutineInvocationStatus::Succeeded,
            ),
            (
                ClientRoutineEventKind::InvocationFailed,
                ClientRoutineInvocationStatus::Failed,
            ),
            (
                ClientRoutineEventKind::InvocationInterrupted,
                ClientRoutineInvocationStatus::Interrupted,
            ),
        ] {
            let mut client = replay_client();
            client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
                ClientRoutineInvocationWatchUpdate::Event(event(4, "invocation", kind, None)),
            )));
            assert_eq!(
                client.state().routines.invocations["invocation"].status,
                expected
            );
        }
    }

    #[test]
    fn summary_only_replay_accepts_task_events_and_updates_only_the_summary() {
        let mut client = Client::new();
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Snapshot(ClientRoutineInvocationSnapshot {
                invocations: vec![summary(
                    "summary-only",
                    41,
                    10,
                    ClientRoutineInvocationStatus::Running,
                )],
                through_sequence: 10,
                older_page_token: None,
            }),
        )));

        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Event(event(
                14,
                "summary-only",
                ClientRoutineEventKind::TaskStarted,
                Some("not-yet-loaded"),
            )),
        )));

        let summary = &client.state().routines.invocations["summary-only"];
        assert_eq!(summary.latest_sequence, 14);
        assert_eq!(summary.updated_at_ms, 14);
        assert_eq!(client.state().routines.through_sequence, Some(14));
        assert!(!client.state().routines.details.contains_key("summary-only"));
    }

    #[test]
    fn restart_interruption_reduces_pending_task_before_invocation_start() {
        let mut client = Client::new();
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Snapshot(ClientRoutineInvocationSnapshot {
                invocations: Vec::new(),
                through_sequence: 2,
                older_page_token: None,
            }),
        )));
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Event(admitted_with_key(7, 101, "restart")),
        )));
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Event(event(
                9,
                "restart",
                ClientRoutineEventKind::TaskInterrupted,
                Some("create"),
            )),
        )));
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Event(event(
                12,
                "restart",
                ClientRoutineEventKind::InvocationInterrupted,
                None,
            )),
        )));

        let detail = client.state().routines.details["restart"]
            .detail
            .as_ref()
            .unwrap();
        assert_eq!(detail.tasks[0].status, ClientRoutineTaskStatus::Interrupted);
        assert_eq!(
            detail.summary.status,
            ClientRoutineInvocationStatus::Interrupted
        );
        assert_eq!(detail.summary.admission_sequence, 101);
        assert_eq!(client.state().routines.through_sequence, Some(12));
    }

    #[test]
    fn separate_admission_and_replay_sequences_are_preserved_after_intervening_events() {
        let mut client = Client::new();
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Snapshot(ClientRoutineInvocationSnapshot {
                invocations: Vec::new(),
                through_sequence: 1,
                older_page_token: None,
            }),
        )));
        for update in [
            admitted_with_key(3, 40, "first"),
            event(5, "first", ClientRoutineEventKind::InvocationStarted, None),
            event(
                8,
                "first",
                ClientRoutineEventKind::InvocationSucceeded,
                None,
            ),
            admitted_with_key(13, 77, "second"),
        ] {
            client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
                ClientRoutineInvocationWatchUpdate::Event(update),
            )));
        }

        assert_eq!(client.state().routines.through_sequence, Some(13));
        assert_eq!(
            client.state().routines.invocations["first"].admission_sequence,
            40
        );
        assert_eq!(
            client.state().routines.invocations["second"].admission_sequence,
            77
        );
        assert_eq!(
            client.state().routines.invocations["second"].latest_sequence,
            13
        );
    }

    #[test]
    fn stale_detail_retries_without_regressing_concurrent_watch_state() {
        let mut client = Client::new();
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Snapshot(ClientRoutineInvocationSnapshot {
                invocations: vec![summary(
                    "concurrent",
                    50,
                    10,
                    ClientRoutineInvocationStatus::Running,
                )],
                through_sequence: 10,
                older_page_token: None,
            }),
        )));
        client.load_routine_invocation_detail("initial-detail", "concurrent");
        client.apply(ClientEvent::Routine(RoutineEvent::DetailLoaded {
            request_id: "initial-detail".to_owned(),
            invocation_id: "concurrent".to_owned(),
            detail: detail(summary(
                "concurrent",
                50,
                10,
                ClientRoutineInvocationStatus::Running,
            )),
        }));
        client.load_routine_invocation_detail("detail-request", "concurrent");
        let mut terminal = event(
            14,
            "concurrent",
            ClientRoutineEventKind::InvocationSucceeded,
            None,
        );
        terminal.values = vec![crate::ClientRoutineResult {
            name: "destination".to_owned(),
            value: crate::ClientRoutineValue::String("session-2".to_owned()),
        }];
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Event(terminal),
        )));

        let update = client.apply(ClientEvent::Routine(RoutineEvent::DetailLoaded {
            request_id: "detail-request".to_owned(),
            invocation_id: "concurrent".to_owned(),
            detail: detail(summary(
                "concurrent",
                50,
                10,
                ClientRoutineInvocationStatus::Running,
            )),
        }));
        assert!(matches!(
            &update.effects()[0],
            ClientEffect::Routine(RoutineEffect::GetInvocation {
                request_id,
                invocation_id,
            }) if request_id == "detail-request" && invocation_id == "concurrent"
        ));
        assert_eq!(
            client.state().routines.invocations["concurrent"].status,
            ClientRoutineInvocationStatus::Succeeded
        );
        assert_eq!(
            client.state().routines.details["concurrent"].status,
            OperationStatus::Loading
        );
        assert_eq!(
            client.state().routines.details["concurrent"]
                .detail
                .as_ref()
                .unwrap()
                .results[0]
                .name,
            "destination"
        );

        let mut current_detail = detail(client.state().routines.invocations["concurrent"].clone());
        current_detail.results = vec![crate::ClientRoutineResult {
            name: "destination".to_owned(),
            value: crate::ClientRoutineValue::String("session-2".to_owned()),
        }];
        client.apply(ClientEvent::Routine(RoutineEvent::DetailLoaded {
            request_id: "detail-request".to_owned(),
            invocation_id: "concurrent".to_owned(),
            detail: current_detail,
        }));
        assert_eq!(
            client.state().routines.details["concurrent"].status,
            OperationStatus::Ready
        );
        assert_eq!(
            client.state().routines.invocations["concurrent"].status,
            ClientRoutineInvocationStatus::Succeeded
        );
    }

    #[test]
    fn future_detail_stays_independent_while_watch_events_catch_up() {
        let mut client = Client::new();
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Snapshot(ClientRoutineInvocationSnapshot {
                invocations: vec![summary(
                    "future",
                    50,
                    10,
                    ClientRoutineInvocationStatus::Admitted,
                )],
                through_sequence: 10,
                older_page_token: None,
            }),
        )));
        client.load_routine_invocation_detail("future-detail", "future");

        let task_result = crate::ClientRoutineResult {
            name: "helper".to_owned(),
            value: crate::ClientRoutineValue::String("session-helper".to_owned()),
        };
        let terminal_result = crate::ClientRoutineResult {
            name: "destination".to_owned(),
            value: crate::ClientRoutineValue::String("session-destination".to_owned()),
        };
        let mut future_summary =
            summary("future", 50, 14, ClientRoutineInvocationStatus::Succeeded);
        future_summary.started_at_ms = Some(11);
        future_summary.completed_at_ms = Some(14);
        future_summary.updated_at_ms = 14;
        let mut future_detail = detail(future_summary.clone());
        future_detail.tasks[0].status = ClientRoutineTaskStatus::Succeeded;
        future_detail.tasks[0].results = vec![task_result.clone()];
        future_detail.results = vec![terminal_result.clone()];
        let update = client.apply(ClientEvent::Routine(RoutineEvent::DetailLoaded {
            request_id: "future-detail".to_owned(),
            invocation_id: "future".to_owned(),
            detail: future_detail,
        }));
        assert!(update.effects().is_empty());
        assert_eq!(client.state().routines.through_sequence, Some(10));
        assert_eq!(
            client.state().routines.invocations["future"].latest_sequence,
            10
        );
        assert_eq!(
            client.state().routines.details["future"]
                .detail
                .as_ref()
                .unwrap()
                .summary
                .latest_sequence,
            14
        );

        let mut task_succeeded = event(
            13,
            "future",
            ClientRoutineEventKind::TaskSucceeded,
            Some("create"),
        );
        task_succeeded.values = vec![task_result];
        let mut invocation_succeeded = event(
            14,
            "future",
            ClientRoutineEventKind::InvocationSucceeded,
            None,
        );
        invocation_succeeded.values = vec![terminal_result];
        for queued in [
            event(
                11,
                "future",
                ClientRoutineEventKind::InvocationStarted,
                None,
            ),
            event(
                12,
                "future",
                ClientRoutineEventKind::TaskStarted,
                Some("create"),
            ),
            task_succeeded,
            invocation_succeeded,
        ] {
            let update = client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
                ClientRoutineInvocationWatchUpdate::Event(queued),
            )));
            assert!(update.effects().is_empty());
        }

        assert_eq!(client.state().routines.through_sequence, Some(14));
        let map = &client.state().routines.invocations["future"];
        let detail = client.state().routines.details["future"]
            .detail
            .as_ref()
            .unwrap();
        assert_eq!(map, &detail.summary);
        assert_eq!(detail.tasks[0].status, ClientRoutineTaskStatus::Succeeded);
        assert_eq!(detail.results[0].name, "destination");
        assert!(!client.state().routines.force_invocation_snapshot);
    }

    #[test]
    fn snapshot_does_not_regress_a_newer_retained_detail() {
        let mut client = Client::new();
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Snapshot(ClientRoutineInvocationSnapshot {
                invocations: vec![summary(
                    "snapshot-race",
                    60,
                    10,
                    ClientRoutineInvocationStatus::Running,
                )],
                through_sequence: 10,
                older_page_token: None,
            }),
        )));
        client.load_routine_invocation_detail("snapshot-detail", "snapshot-race");
        let future = detail(summary(
            "snapshot-race",
            60,
            14,
            ClientRoutineInvocationStatus::Succeeded,
        ));
        client.apply(ClientEvent::Routine(RoutineEvent::DetailLoaded {
            request_id: "snapshot-detail".to_owned(),
            invocation_id: "snapshot-race".to_owned(),
            detail: future,
        }));

        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Snapshot(ClientRoutineInvocationSnapshot {
                invocations: vec![summary(
                    "snapshot-race",
                    60,
                    12,
                    ClientRoutineInvocationStatus::Running,
                )],
                through_sequence: 12,
                older_page_token: None,
            }),
        )));

        assert_eq!(
            client.state().routines.invocations["snapshot-race"].latest_sequence,
            12
        );
        assert_eq!(
            client.state().routines.details["snapshot-race"]
                .detail
                .as_ref()
                .unwrap()
                .summary
                .latest_sequence,
            14
        );
    }

    #[test]
    fn future_detail_without_a_map_item_retries_only_to_a_fixed_limit() {
        let mut client = Client::new();
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Snapshot(ClientRoutineInvocationSnapshot {
                invocations: Vec::new(),
                through_sequence: 10,
                older_page_token: None,
            }),
        )));
        client.load_routine_invocation_detail("missing-detail", "missing");
        let future = detail(summary(
            "missing",
            70,
            14,
            ClientRoutineInvocationStatus::Running,
        ));

        for attempt in 1..=MAX_FUTURE_DETAIL_RETRIES {
            let update = client.apply(ClientEvent::Routine(RoutineEvent::DetailLoaded {
                request_id: "missing-detail".to_owned(),
                invocation_id: "missing".to_owned(),
                detail: future.clone(),
            }));
            if attempt < MAX_FUTURE_DETAIL_RETRIES {
                assert!(matches!(
                    &update.effects()[0],
                    ClientEffect::Routine(RoutineEffect::GetInvocation { .. })
                ));
            } else {
                assert!(update.effects().is_empty());
            }
        }
        assert!(!client.state().routines.invocations.contains_key("missing"));
        assert_eq!(
            client.state().routines.details["missing"].status,
            OperationStatus::Failed
        );
        assert!(client.state().routines.details["missing"].error.is_some());
    }

    #[test]
    fn terminal_regression_forces_snapshot_without_advancing_replay_cursor() {
        let mut client = replay_client();
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Event(event(
                4,
                "invocation",
                ClientRoutineEventKind::InvocationSucceeded,
                None,
            )),
        )));
        assert_eq!(client.state().routines.through_sequence, Some(4));
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Event(event(
                7,
                "invocation",
                ClientRoutineEventKind::InvocationStarted,
                None,
            )),
        )));
        assert_eq!(client.state().routines.through_sequence, None);
        assert_eq!(
            client.state().routines.invocations["invocation"].status,
            ClientRoutineInvocationStatus::Succeeded
        );
    }

    #[test]
    fn transient_access_preserves_interests_and_logout_clears_everything() {
        let mut client = Client::new();
        drop(client.register_routine_definition_watch_interest());
        drop(client.register_routine_invocation_watch_interest());
        drop(client.register_session_routine_invocation_watch_interest("source"));
        client.expire_waltr_access("expired");
        assert_eq!(client.state().access.status, AccessStatus::LoginRequired);
        assert_eq!(client.state().routines.definition_watch.interests, 1);
        assert_eq!(client.state().routines.invocation_watch.interests, 1);
        assert_eq!(
            client.state().routines.session_histories["source"]
                .watch
                .interests,
            1
        );

        client.logout_waltr();
        assert_eq!(
            client.state().routines,
            crate::RoutineClientState::default()
        );
    }

    #[test]
    fn local_start_operations_are_pruned_to_the_declared_bound() {
        let mut client = Client::new();
        for index in 0..(MAX_RETAINED_ROUTINE_STARTS + 5) {
            client.start_manual_routine(
                format!("operation-{index}"),
                format!("request-{index}"),
                "fork",
                "manual",
                "source",
                Default::default(),
            );
        }
        assert_eq!(
            client.state().routines.starts.len(),
            MAX_RETAINED_ROUTINE_STARTS
        );
        assert!(!client.state().routines.starts.contains_key("operation-0"));
    }

    #[test]
    fn stale_open_identity_cannot_open_the_definition_watch() {
        let mut client = Client::new();
        client.apply(ClientEvent::Status(crate::StatusEvent::WatchStarted));
        client.apply(ClientEvent::Status(crate::StatusEvent::WatchChanged(
            crate::SystemStatus { ready: true },
        )));
        let update = client.register_routine_definition_watch_interest();
        let open_id = match &update.effects()[0] {
            ClientEffect::Routine(RoutineEffect::WatchDefinitions { open_id, .. }) => *open_id,
            _ => panic!("definition watch should open"),
        };

        client.apply(ClientEvent::Routine(
            RoutineEvent::DefinitionWatchStartedForOpen {
                open_id: open_id.saturating_add(1),
            },
        ));
        assert_eq!(
            client.state().routines.definition_watch.status,
            OperationStatus::Loading
        );
        assert_eq!(
            client.state().routines.definition_watch.open_id,
            Some(open_id)
        );
    }

    #[test]
    fn session_invocation_watch_reconnects_by_key_and_release_removes_only_that_history() {
        let mut state = ClientState::default();
        state.access.status = AccessStatus::Authenticated;
        state.status.current = Some(crate::SystemStatus { ready: true });
        state.status.watch.status = OperationStatus::Received;
        for session_id in ["one", "two"] {
            let history = state
                .routines
                .session_histories
                .entry(session_id.to_owned())
                .or_default();
            history.watch.interests = 1;
            history.watch.status = OperationStatus::Failed;
            history.through_sequence = Some(17);
        }

        let update = maintain_session_invocation_watch(&mut state, "one");
        let open_id = match &update.effects()[0] {
            ClientEffect::Routine(RoutineEffect::WatchInvocations {
                open_id,
                after_sequence: Some(17),
                metadata_filters,
                session_id: Some(session_id),
                ..
            }) => {
                assert_eq!(session_id, "one");
                assert_eq!(
                    metadata_filters[crate::ROUTINE_SOURCE_SESSION_ID_METADATA_KEY],
                    "one"
                );
                *open_id
            }
            effect => panic!("unexpected effect: {effect:?}"),
        };
        reduce(
            &mut state,
            RoutineEvent::SessionInvocationWatchStartedForOpen {
                session_id: "one".to_owned(),
                open_id: open_id.saturating_add(1),
            },
        );
        assert_eq!(
            state.routines.session_histories["one"].watch.open_id,
            Some(open_id)
        );

        reduce(
            &mut state,
            RoutineEvent::SessionInvocationWatchInterestReleased {
                session_id: "one".to_owned(),
            },
        );
        assert!(!state.routines.session_histories.contains_key("one"));
        assert!(state.routines.session_histories.contains_key("two"));
    }

    #[test]
    fn invocation_reconnect_replays_after_last_reduced_cursor_and_recovery_forces_snapshot() {
        let mut state = ClientState::default();
        state.access.status = AccessStatus::Authenticated;
        state.status.current = Some(crate::SystemStatus { ready: true });
        state.status.watch.status = OperationStatus::Received;
        state.routines.invocation_watch.interests = 1;
        state.routines.invocation_watch.status = OperationStatus::Failed;
        state.routines.through_sequence = Some(17);

        let update = maintain_invocation_watch(&mut state);
        assert!(matches!(
            &update.effects()[0],
            ClientEffect::Routine(RoutineEffect::WatchInvocations {
                after_sequence: Some(17),
                ..
            })
        ));

        state.routines.invocation_watch.status = OperationStatus::Waiting;
        state.routines.invocation_watch.open_id = None;
        state.routines.force_invocation_snapshot = true;
        let update = maintain_invocation_watch(&mut state);
        assert!(matches!(
            &update.effects()[0],
            ClientEffect::Routine(RoutineEffect::WatchInvocations {
                after_sequence: None,
                ..
            })
        ));
    }

    #[test]
    fn live_admission_retention_is_bounded_and_preserves_page_continuity() {
        let mut client = Client::new();
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Snapshot(ClientRoutineInvocationSnapshot {
                invocations: Vec::new(),
                through_sequence: 1,
                older_page_token: Some(50),
            }),
        )));
        for index in 0..(MAX_RETAINED_ROUTINE_INVOCATIONS + 5) {
            let sequence = 3 + u64::try_from(index).unwrap() * 3;
            let admission_sequence = 100 + u64::try_from(index).unwrap() * 10;
            client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
                ClientRoutineInvocationWatchUpdate::Event(admitted_with_key(
                    sequence,
                    admission_sequence,
                    &format!("live-{index}"),
                )),
            )));
        }
        assert_eq!(
            client.state().routines.invocations.len(),
            MAX_RETAINED_ROUTINE_INVOCATIONS
        );
        assert!(!client.state().routines.invocations.contains_key("live-4"));
        assert!(client.state().routines.invocations.contains_key("live-5"));
        assert_eq!(client.state().routines.older_page_token, Some(150));
    }

    #[test]
    fn bounded_start_and_detail_pins_survive_history_eviction_and_keep_reducing() {
        let mut state = ClientState::default();
        for index in 0..(MAX_RETAINED_ROUTINE_INVOCATIONS + 5) {
            let id = format!("pinned-{index}");
            state.routines.invocations.insert(
                id.clone(),
                summary(
                    &id,
                    10 + u64::try_from(index).unwrap() * 10,
                    10 + u64::try_from(index).unwrap(),
                    ClientRoutineInvocationStatus::Running,
                ),
            );
        }
        state.routines.starts.insert(
            "start-pin".to_owned(),
            RoutineStartOperationState {
                operation_key: "start-pin".to_owned(),
                invocation_id: Some("pinned-0".to_owned()),
                status: OperationStatus::Ready,
                ..RoutineStartOperationState::default()
            },
        );
        state.routines.start_order.push_back("start-pin".to_owned());
        let explicit = state.routines.invocations["pinned-1"].clone();
        state.routines.details.insert(
            "pinned-1".to_owned(),
            crate::RoutineDetailState {
                request_id: "detail-pin".to_owned(),
                status: OperationStatus::Ready,
                detail: Some(detail(explicit)),
                error: None,
                future_retry_count: 0,
            },
        );
        state.routines.detail_order.push_back("pinned-1".to_owned());

        bound_invocations(&mut state);
        assert_eq!(
            state.routines.invocations.len(),
            MAX_RETAINED_ROUTINE_INVOCATIONS + 2
        );
        assert!(state.routines.invocations.contains_key("pinned-0"));
        assert!(state.routines.invocations.contains_key("pinned-1"));

        reduce_invocation_event(
            &mut state.routines,
            &event(
                1_000,
                "pinned-1",
                ClientRoutineEventKind::TaskStarted,
                Some("create"),
            ),
        )
        .unwrap();
        assert_eq!(
            state.routines.details["pinned-1"]
                .detail
                .as_ref()
                .unwrap()
                .tasks[0]
                .status,
            ClientRoutineTaskStatus::Running
        );
        assert_eq!(
            state.routines.invocations["pinned-1"].latest_sequence,
            1_000
        );
    }

    #[test]
    fn explicitly_loaded_detail_pins_are_bounded() {
        let mut client = Client::new();
        for index in 0..(MAX_RETAINED_ROUTINE_DETAILS + 5) {
            client.load_routine_invocation_detail(
                format!("request-{index}"),
                format!("detail-{index}"),
            );
        }
        assert_eq!(
            client.state().routines.details.len(),
            MAX_RETAINED_ROUTINE_DETAILS
        );
        assert!(!client.state().routines.details.contains_key("detail-0"));
    }

    #[test]
    fn gappy_replay_advances_cursor_only_after_successful_reduction() {
        let mut client = Client::new();
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Snapshot(ClientRoutineInvocationSnapshot {
                invocations: Vec::new(),
                through_sequence: 2,
                older_page_token: None,
            }),
        )));
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Event(admitted(7, "invocation")),
        )));
        assert_eq!(client.state().routines.through_sequence, Some(7));
        assert_eq!(
            client.state().routines.invocations["invocation"].admission_sequence,
            7
        );

        let mut malformed = admitted(9, "invocation");
        malformed.kind = ClientRoutineEventKind::TaskStarted;
        malformed.admission = None;
        malformed.task_name = Some("missing".to_owned());
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Event(malformed),
        )));
        assert_eq!(client.state().routines.through_sequence, None);
        assert!(client.state().routines.force_invocation_snapshot);
    }

    #[test]
    fn zero_admission_sequence_is_rejected_without_cursor_advancement() {
        let mut client = Client::new();
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Snapshot(ClientRoutineInvocationSnapshot {
                invocations: Vec::new(),
                through_sequence: 2,
                older_page_token: None,
            }),
        )));
        let mut malformed = admitted(7, "malformed");
        malformed
            .admission
            .as_mut()
            .expect("admission metadata")
            .admission_sequence = 0;
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            ClientRoutineInvocationWatchUpdate::Event(malformed),
        )));

        assert_ne!(client.state().routines.through_sequence, Some(7));
        assert!(
            !client
                .state()
                .routines
                .invocations
                .contains_key("malformed")
        );
        assert!(client.state().routines.force_invocation_snapshot);
    }
}
