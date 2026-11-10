//! Deterministic v2 observation coordination. No execution replay lives here.

use crate::*;
use std::collections::{BTreeMap, BTreeSet};

pub fn reduce(state: &mut ClientState, event: RoutineEvent) -> ClientUpdate {
    match event {
        RoutineEvent::RequestReleased {
            operation_key,
            request_id,
        } => {
            if state
                .routines
                .requests
                .get(&operation_key)
                .is_some_and(|r| r.request_id == request_id)
            {
                state.routines.requests.remove(&operation_key);
                state
                    .routines
                    .selected_page_order
                    .retain(|key| key != &operation_key);
            }
            ClientUpdate::default()
        }
        RoutineEvent::Request {
            operation_key,
            request_id,
            command,
        } => {
            if state.routines.requests.len() >= 32
                && !state.routines.requests.contains_key(&operation_key)
            {
                if let Some(key) = state
                    .routines
                    .requests
                    .iter()
                    .find(|(_, r)| r.status != RoutineRequestStatus::Loading)
                    .map(|(k, _)| k.clone())
                {
                    state.routines.requests.remove(&key);
                } else {
                    return ClientUpdate::default();
                }
            }
            let invalid = match &command {
                RoutineRequest::Invocations {
                    limit,
                    source_session,
                    ..
                } => {
                    !(1..=100).contains(limit)
                        || source_session.as_ref().is_some_and(|s| s.is_empty())
                }
                RoutineRequest::Operations {
                    invocation_id,
                    limit,
                    ..
                } => {
                    !(1..=100).contains(limit)
                        || state
                            .routines
                            .details
                            .get(invocation_id)
                            .is_none_or(|d| d.detail.is_none())
                }
                RoutineRequest::Cancel { invocation_id } => invocation_id.is_empty(),
            };
            state.routines.requests.insert(
                operation_key.clone(),
                RoutineRequestState {
                    request_id: request_id.clone(),
                    command: command.clone(),
                    status: if invalid {
                        RoutineRequestStatus::Failed
                    } else {
                        RoutineRequestStatus::Loading
                    },
                    error: invalid.then(|| "invalid Routine request".into()),
                    next_cursor: None,
                    cancellation_status: None,
                    selected_invocations: vec![],
                },
            );
            if invalid {
                ClientUpdate::default()
            } else {
                ClientUpdate::with_effect(ClientEffect::Routine(RoutineEffect::Request {
                    operation_key,
                    request_id,
                    command,
                }))
            }
        }
        RoutineEvent::Requested {
            operation_key,
            request_id,
            result,
        } => {
            let Some(receipt) = state.routines.requests.get(&operation_key).filter(|r| {
                r.request_id == request_id && r.status == RoutineRequestStatus::Loading
            }) else {
                return ClientUpdate::default();
            };
            let command = receipt.command.clone();
            let mut next_cursor = None;
            let mut cancellation_status = None;
            let result = match (command, result) {
                (
                    RoutineRequest::Invocations {
                        source_session,
                        after_cursor,
                        limit,
                    },
                    RoutineRequestResult::Invocations(page),
                ) => {
                    next_cursor = page.older_page_token.map(RoutineRevision);
                    let mut selected = page.invocations;
                    let mut previous = after_cursor;
                    let result = selected.iter().try_for_each(|row| {
                        if selected.len() > limit as usize || row.admission_sequence <= previous {
                            return Err("invalid selected page ordering or count".into());
                        }
                        previous = row.admission_sequence;
                        if source_session
                            .as_ref()
                            .is_some_and(|s| row.source_session.as_ref() != Some(s))
                        {
                            return Err("source filter response mismatch".into());
                        }
                        merge_selected_row(&mut state.routines, row.clone())
                    });
                    if result.is_ok() {
                        for row in &mut selected {
                            if let Some(current) =
                                state.routines.invocations.get(&row.invocation_id)
                            {
                                row.clone_from(current);
                            }
                        }
                        state
                            .routines
                            .requests
                            .get_mut(&operation_key)
                            .unwrap()
                            .selected_invocations = selected;
                        state
                            .routines
                            .selected_page_order
                            .retain(|key| key != &operation_key);
                        state
                            .routines
                            .selected_page_order
                            .push_back(operation_key.clone());
                    }
                    result
                }
                (
                    RoutineRequest::Operations { invocation_id, .. },
                    RoutineRequestResult::Operations {
                        invocation_id: returned,
                        operations,
                        next_cursor: cursor,
                    },
                ) if invocation_id == returned => {
                    next_cursor = cursor;
                    let result = operations.iter().try_for_each(|op| {
                        if op.invocation_id != invocation_id {
                            Err("operation identity mismatch".into())
                        } else {
                            merge_operation(&mut state.routines.operations, op.clone())
                        }
                    });
                    if result.is_ok()
                        && let Some(detail) = state
                            .routines
                            .details
                            .get_mut(&invocation_id)
                            .and_then(|d| d.detail.as_mut())
                    {
                        detail.operations = operations;
                        detail.next_operation_cursor = cursor;
                    }
                    result
                }
                (RoutineRequest::Cancel { .. }, RoutineRequestResult::Cancelled(status)) => {
                    cancellation_status = Some(status);
                    Ok(())
                }
                _ => Err("Routine response shape mismatch".into()),
            };
            let receipt = state.routines.requests.get_mut(&operation_key).unwrap();
            receipt.status = if result.is_ok() {
                RoutineRequestStatus::Ready
            } else {
                RoutineRequestStatus::Failed
            };
            receipt.error = result.err();
            receipt.next_cursor = next_cursor;
            receipt.cancellation_status = cancellation_status;
            sync_histories(state);
            sync_details(state);
            bound(state);
            ClientUpdate::default()
        }
        RoutineEvent::RequestFailed {
            operation_key,
            request_id,
            error,
        } => {
            if let Some(receipt) =
                state.routines.requests.get_mut(&operation_key).filter(|r| {
                    r.request_id == request_id && r.status == RoutineRequestStatus::Loading
                })
            {
                receipt.status = RoutineRequestStatus::Failed;
                receipt.error = Some(error.to_string());
            }
            ClientUpdate::default()
        }
        RoutineEvent::DefinitionWatchInterestRegistered => {
            state.routines.definition_watch.interests =
                state.routines.definition_watch.interests.saturating_add(1);
            maintain_definition_watch(state)
        }
        RoutineEvent::DefinitionWatchInterestReleased => {
            release(&mut state.routines.definition_watch);
            ClientUpdate::default()
        }
        RoutineEvent::DefinitionWatchStartedForOpen { open_id } => {
            opened(&mut state.routines.definition_watch, open_id);
            ClientUpdate::default()
        }
        RoutineEvent::DefinitionWatchOpenFailed { open_id, error } => {
            if state.routines.definition_watch.open_id == Some(open_id) {
                failed(state, None, true, error.to_string())
            } else {
                ClientUpdate::default()
            }
        }
        RoutineEvent::DefinitionWatchChanged(s) => {
            if !state.routines.definition_digest.is_empty()
                && state.routines.definition_digest == s.digest
                && (state.routines.definitions != s.definitions
                    || state.routines.supported_capabilities != s.supported_capabilities
                    || state.routines.limits.as_ref() != Some(&s.limits))
            {
                return failed(
                    state,
                    None,
                    true,
                    "catalog revision reused with different fields".into(),
                );
            }
            state.routines.definition_digest = s.digest;
            state.routines.definitions = s.definitions;
            state.routines.supported_capabilities = s.supported_capabilities;
            state.routines.limits = Some(s.limits);
            received(&mut state.routines.definition_watch);
            ClientUpdate::default()
        }
        RoutineEvent::DefinitionWatchFailed(e) => failed(state, None, true, e.to_string()),
        RoutineEvent::DefinitionWatchEnded => {
            failed(state, None, true, "definition watch ended".into())
        }
        RoutineEvent::DefinitionReconnectReady => maintain_definition_watch(state),
        RoutineEvent::InvocationWatchInterestRegistered => {
            state.routines.invocation_watch.interests =
                state.routines.invocation_watch.interests.saturating_add(1);
            maintain_invocation_watch(state)
        }
        RoutineEvent::InvocationWatchInterestReleased => {
            release(&mut state.routines.invocation_watch);
            ClientUpdate::default()
        }
        RoutineEvent::InvocationWatchStartedForOpen { open_id } => {
            opened(&mut state.routines.invocation_watch, open_id);
            ClientUpdate::default()
        }
        RoutineEvent::InvocationWatchOpenFailed { open_id, error } => {
            if state.routines.invocation_watch.open_id == Some(open_id) {
                failed(state, None, false, error.to_string())
            } else {
                ClientUpdate::default()
            }
        }
        RoutineEvent::InvocationWatchChanged(s) => apply_observation(state, None, s),
        RoutineEvent::InvocationWatchFailed(e) => failed(state, None, false, e.to_string()),
        RoutineEvent::InvocationWatchEnded => {
            failed(state, None, false, "invocation watch ended".into())
        }
        RoutineEvent::InvocationReconnectReady => maintain_invocation_watch(state),
        RoutineEvent::SessionInvocationWatchInterestRegistered { session_id } => {
            if session_id.is_empty() || session_id.len() > 256 {
                return ClientUpdate::default();
            }
            let h = state
                .routines
                .session_histories
                .entry(session_id.clone())
                .or_default();
            h.watch.interests = h.watch.interests.saturating_add(1);
            maintain_session_invocation_watch(state, &session_id)
        }
        RoutineEvent::SessionInvocationWatchInterestReleased { session_id } => {
            if let Some(h) = state.routines.session_histories.get_mut(&session_id) {
                release(&mut h.watch);
                if h.watch.interests == 0 {
                    state.routines.session_histories.remove(&session_id);
                }
            }
            ClientUpdate::default()
        }
        RoutineEvent::SessionInvocationWatchStartedForOpen {
            session_id,
            open_id,
        } => {
            if let Some(h) = state.routines.session_histories.get_mut(&session_id) {
                opened(&mut h.watch, open_id);
            }
            ClientUpdate::default()
        }
        RoutineEvent::SessionInvocationWatchOpenFailed {
            session_id,
            open_id,
            error,
        } => {
            if state
                .routines
                .session_histories
                .get(&session_id)
                .is_some_and(|h| h.watch.open_id == Some(open_id))
            {
                failed(state, Some(&session_id), false, error.to_string())
            } else {
                ClientUpdate::default()
            }
        }
        RoutineEvent::SessionInvocationWatchChanged { session_id, update } => {
            apply_observation(state, Some(&session_id), update)
        }
        RoutineEvent::SessionInvocationWatchFailed { session_id, error } => {
            failed(state, Some(&session_id), false, error.to_string())
        }
        RoutineEvent::SessionInvocationWatchEnded { session_id } => failed(
            state,
            Some(&session_id),
            false,
            "session observation ended".into(),
        ),
        RoutineEvent::SessionInvocationReconnectReady { session_id } => {
            maintain_session_invocation_watch(state, &session_id)
        }
        RoutineEvent::StartRequested {
            operation_key,
            request_id,
            routine_name,
            routine_revision,
            trigger_name,
            source_session_id,
            inputs,
        } => {
            let valid = state
                .routines
                .definitions
                .iter()
                .find(|d| d.routine_name == routine_name && d.routine_revision == routine_revision)
                .is_some_and(|d| {
                    d.permits(&trigger_name, false, source_session_id.as_deref())
                        && d.parameters.validate(&inputs).is_ok()
                });
            state.routines.starts.insert(
                operation_key.clone(),
                RoutineStartOperationState {
                    operation_key: operation_key.clone(),
                    request_id: request_id.clone(),
                    routine_name: routine_name.clone(),
                    trigger_name: trigger_name.clone(),
                    source_session_id: source_session_id.clone(),
                    inputs: Some(inputs.clone()),
                    status: if valid {
                        OperationStatus::Loading
                    } else {
                        OperationStatus::Failed
                    },
                    error: (!valid)
                        .then(|| "selection is stale, ineligible or parameters are invalid".into()),
                    ..Default::default()
                },
            );
            touch(&mut state.routines.start_order, &operation_key);
            while state.routines.start_order.len() > MAX_RETAINED_ROUTINE_STARTS {
                if let Some(id) = state.routines.start_order.pop_front() {
                    state.routines.starts.remove(&id);
                }
            }
            if !valid {
                return ClientUpdate::default();
            }
            ClientUpdate::with_effect(ClientEffect::Routine(RoutineEffect::StartManual {
                operation_key,
                request_id,
                routine_name,
                routine_revision,
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
            if let Some(s) = current_start(state, &operation_key, &request_id) {
                s.status = OperationStatus::Ready;
                s.invocation_id = Some(invocation_id);
            }
            bound(state);
            ClientUpdate::default()
        }
        RoutineEvent::StartValidationFailed {
            operation_key,
            request_id,
            failure,
        } => {
            if let Some(s) = current_start(state, &operation_key, &request_id) {
                s.status = OperationStatus::Failed;
                s.validation_failure = Some(failure);
            }
            ClientUpdate::default()
        }
        RoutineEvent::StartFailed {
            operation_key,
            request_id,
            error,
        } => {
            if let Some(s) = current_start(state, &operation_key, &request_id) {
                s.status = OperationStatus::Failed;
                s.error = Some(error.to_string());
            }
            ClientUpdate::default()
        }
        RoutineEvent::OlderPageRequested {
            request_id,
            page_size,
            before_admission_sequence,
        } => {
            state.routines.page = RoutinePageState {
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
            if state.routines.page.request_id != request_id
                || state.routines.page.before_admission_sequence != Some(before_admission_sequence)
                || state.routines.page.status != OperationStatus::Loading
            {
                return ClientUpdate::default();
            }
            for row in page.invocations {
                if let Err(e) = merge_selected_row(&mut state.routines, row) {
                    state.routines.page.status = OperationStatus::Failed;
                    state.routines.page.error = Some(e);
                    return ClientUpdate::default();
                }
            }
            state.routines.older_page_token = page.older_page_token;
            state.routines.page.status = OperationStatus::Ready;
            bound(state);
            ClientUpdate::default()
        }
        RoutineEvent::OlderPageFailed {
            request_id,
            before_admission_sequence,
            error,
        } => {
            if state.routines.page.request_id == request_id
                && state.routines.page.before_admission_sequence == Some(before_admission_sequence)
            {
                state.routines.page.status = OperationStatus::Failed;
                state.routines.page.error = Some(error.to_string());
            }
            ClientUpdate::default()
        }
        RoutineEvent::DetailRequested {
            request_id,
            invocation_id,
        } => {
            state.routines.details.insert(
                invocation_id.clone(),
                RoutineDetailState {
                    request_id: request_id.clone(),
                    status: OperationStatus::Loading,
                    ..Default::default()
                },
            );
            touch(&mut state.routines.detail_order, &invocation_id);
            while state.routines.detail_order.len() > MAX_RETAINED_ROUTINE_DETAILS {
                if let Some(id) = state.routines.detail_order.pop_front() {
                    state.routines.details.remove(&id);
                }
            }
            bound(state);
            ClientUpdate::with_effect(ClientEffect::Routine(RoutineEffect::GetInvocation {
                request_id,
                invocation_id,
            }))
        }
        RoutineEvent::DetailLoaded {
            request_id,
            invocation_id,
            mut detail,
        } => {
            if !state
                .routines
                .details
                .get(&invocation_id)
                .is_some_and(|d| d.request_id == request_id && d.status == OperationStatus::Loading)
            {
                return ClientUpdate::default();
            }
            let error = if detail.summary.invocation_id != invocation_id {
                Some("detail identity mismatch".into())
            } else {
                merge_selected_row(&mut state.routines, detail.summary.clone()).err()
            };
            if let Some(error) = error {
                let d = state.routines.details.get_mut(&invocation_id).unwrap();
                d.status = OperationStatus::Failed;
                d.error = Some(error);
                return ClientUpdate::default();
            }
            // A delayed Get cannot roll a visible row back. The immutable definition
            // remains pinned even after removal from the live catalog.
            detail
                .summary
                .clone_from(&state.routines.invocations[&invocation_id]);
            for op in &detail.operations {
                if let Err(error) = merge_operation(&mut state.routines.operations, op.clone()) {
                    let d = state.routines.details.get_mut(&invocation_id).unwrap();
                    d.status = OperationStatus::Failed;
                    d.error = Some(error);
                    return ClientUpdate::default();
                }
            }
            let d = state.routines.details.get_mut(&invocation_id).unwrap();
            d.status = OperationStatus::Ready;
            d.detail = Some(*detail);
            d.error = None;
            sync_histories(state);
            sync_details(state);
            bound(state);
            ClientUpdate::default()
        }
        RoutineEvent::DetailFailed {
            request_id,
            invocation_id,
            error,
        } => {
            if let Some(d) = state.routines.details.get_mut(&invocation_id)
                && d.request_id == request_id
                && d.status == OperationStatus::Loading
            {
                d.status = OperationStatus::Failed;
                d.error = Some(error.to_string());
            }
            ClientUpdate::default()
        }
    }
}

fn current_start<'a>(
    s: &'a mut ClientState,
    key: &str,
    request: &str,
) -> Option<&'a mut RoutineStartOperationState> {
    s.routines
        .starts
        .get_mut(key)
        .filter(|s| s.request_id == request && s.status == OperationStatus::Loading)
}
fn touch(order: &mut std::collections::VecDeque<String>, id: &str) {
    order.retain(|v| v != id);
    order.push_back(id.into());
}

fn merge_row(
    rows: &mut BTreeMap<String, ClientRoutineInvocationSummary>,
    row: ClientRoutineInvocationSummary,
) -> Result<(), String> {
    if let Some(old) = rows.get(&row.invocation_id) {
        if old.observation_revision > row.observation_revision {
            return Ok(());
        }
        if old.observation_revision == row.observation_revision {
            return if old == &row {
                Ok(())
            } else {
                Err("same observation revision has different fields".into())
            };
        }
        if old.admission_sequence != row.admission_sequence
            || old.routine_revision != row.routine_revision
            || old.routine_name != row.routine_name
            || old.trigger_name != row.trigger_name
            || old.source_session != row.source_session
        {
            return Err("immutable invocation identity changed".into());
        }
    }
    rows.insert(row.invocation_id.clone(), row);
    Ok(())
}
fn merge_selected_row(
    s: &mut RoutineClientState,
    row: ClientRoutineInvocationSummary,
) -> Result<(), String> {
    for selected in s
        .requests
        .values()
        .flat_map(|r| r.selected_invocations.iter())
        .filter(|r| r.invocation_id == row.invocation_id)
    {
        merge_row(&mut s.invocations, selected.clone())?;
    }
    merge_row(&mut s.invocations, row)
}
fn merge_operation(
    rows: &mut BTreeMap<String, RoutineOperation>,
    row: RoutineOperation,
) -> Result<(), String> {
    if let Some(old) = rows.get(&row.id) {
        if old.observation_revision > row.observation_revision {
            return Ok(());
        }
        if old.observation_revision == row.observation_revision {
            return if old == &row {
                Ok(())
            } else {
                Err("operation revision changed fields".into())
            };
        }
        if old.invocation_id != row.invocation_id || old.cursor != row.cursor {
            return Err("operation identity changed".into());
        }
    }
    rows.insert(row.id.clone(), row);
    Ok(())
}

fn apply_observation(
    state: &mut ClientState,
    source: Option<&str>,
    s: ClientRoutineInvocationWatchUpdate,
) -> ClientUpdate {
    if s.phase == RoutineObservationPhase::ResetRequired {
        return reset(state, source, "observation cursor expired");
    }
    let (mut rows, mut ops, old_high, complete, cursor) = if let Some(source) = source {
        let Some(h) = state.routines.session_histories.get(source) else {
            return ClientUpdate::default();
        };
        (
            h.invocations.clone(),
            h.operations.clone(),
            h.snapshot_high_water.clone(),
            h.snapshot_complete,
            h.through_sequence.clone(),
        )
    } else {
        (
            state.routines.invocations.clone(),
            state.routines.operations.clone(),
            state.routines.snapshot_high_water.clone(),
            state.routines.snapshot_complete,
            state.routines.through_sequence.clone(),
        )
    };
    if s.cursor.is_empty()
        || s.cursor.len() > 1024
        || (s.phase == RoutineObservationPhase::Changes && !complete)
        || (s.phase == RoutineObservationPhase::Snapshot
            && (complete
                || s.snapshot_high_water.is_empty()
                || old_high
                    .as_ref()
                    .is_some_and(|h| h != &s.snapshot_high_water)
                || (s.snapshot_complete && s.cursor != s.snapshot_high_water)))
        || s.invocations
            .iter()
            .any(|r| source.is_some_and(|source| r.source_session.as_deref() != Some(source)))
    {
        return reset(
            state,
            source,
            "invalid observation phase, source or snapshot high-water",
        );
    }
    if s.phase == RoutineObservationPhase::Snapshot && cursor.is_none() {
        let pins = pinned(&state.routines);
        rows.retain(|id, _| pins.contains(id));
        ops.retain(|_, op| pins.contains(&op.invocation_id));
    }
    let mut canonical_rows = state.routines.invocations.clone();
    for selected in state
        .routines
        .requests
        .values()
        .flat_map(|r| r.selected_invocations.iter())
    {
        if merge_row(&mut canonical_rows, selected.clone()).is_err() {
            return reset(state, source, "inconsistent selected page revision");
        }
    }
    let mut canonical_ops = state.routines.operations.clone();
    for row in s.invocations {
        let id = row.invocation_id.clone();
        if merge_row(&mut canonical_rows, row).is_err()
            || merge_row(&mut rows, canonical_rows[&id].clone()).is_err()
        {
            return reset(state, source, "inconsistent invocation revision");
        }
    }
    for op in s.operations {
        let id = op.id.clone();
        if merge_operation(&mut canonical_ops, op).is_err()
            || merge_operation(&mut ops, canonical_ops[&id].clone()).is_err()
        {
            return reset(state, source, "inconsistent operation revision");
        }
    }
    if let Some(source) = source {
        let h = state.routines.session_histories.get_mut(source).unwrap();
        h.invocations = rows;
        h.operations = ops;
        h.through_sequence = Some(s.cursor);
        if s.phase == RoutineObservationPhase::Snapshot {
            h.snapshot_high_water = Some(s.snapshot_high_water);
            h.snapshot_complete = s.snapshot_complete;
        }
        received(&mut h.watch);
        bound_rows(&mut h.invocations, &BTreeSet::new());
        h.operations
            .retain(|_, op| h.invocations.contains_key(&op.invocation_id));
        let mut counts = BTreeMap::new();
        h.operations.retain(|_, op| {
            let count = counts.entry(op.invocation_id.clone()).or_insert(0usize);
            *count += 1;
            *count <= 100
        });
        state.routines.invocations = canonical_rows;
        state.routines.operations = canonical_ops;
    } else {
        state.routines.invocations = rows;
        state.routines.operations = ops;
        state.routines.through_sequence = Some(s.cursor);
        if s.phase == RoutineObservationPhase::Snapshot {
            state.routines.snapshot_high_water = Some(s.snapshot_high_water);
            state.routines.snapshot_complete = s.snapshot_complete;
        }
        received(&mut state.routines.invocation_watch);
    }
    sync_histories(state);
    sync_details(state);
    bound(state);
    ClientUpdate::default()
}
fn sync_histories(state: &mut ClientState) {
    for (source, history) in &mut state.routines.session_histories {
        for (id, row) in &mut history.invocations {
            if let Some(newer) = state.routines.invocations.get(id)
                && newer.source_session.as_deref() == Some(source)
                && newer.observation_revision > row.observation_revision
            {
                row.clone_from(newer);
            }
        }
        for (id, op) in &mut history.operations {
            if let Some(newer) = state.routines.operations.get(id)
                && newer.observation_revision > op.observation_revision
            {
                op.clone_from(newer);
            }
        }
    }
}
fn sync_details(state: &mut ClientState) {
    for (id, d) in &mut state.routines.details {
        if let Some(detail) = d.detail.as_mut() {
            if let Some(row) = state.routines.invocations.get(id) {
                detail.summary.clone_from(row);
            }
            for operation in &mut detail.operations {
                if let Some(newer) = state.routines.operations.get(&operation.id)
                    && newer.observation_revision >= operation.observation_revision
                {
                    operation.clone_from(newer);
                }
            }
            if detail.next_operation_cursor.is_none() {
                let last = detail
                    .operations
                    .iter()
                    .map(|op| op.cursor)
                    .max()
                    .unwrap_or_default();
                let mut more = state
                    .routines
                    .operations
                    .values()
                    .filter(|op| &op.invocation_id == id && op.cursor > last)
                    .cloned()
                    .collect::<Vec<_>>();
                more.sort_by_key(|op| op.cursor);
                detail.operations.extend(more);
                if detail.operations.len() > 100 {
                    detail.operations.truncate(100);
                    detail.next_operation_cursor = detail.operations.last().map(|op| op.cursor);
                }
            }
            detail.operations.sort_by_key(|op| op.cursor);
        }
    }
}
fn pinned(s: &RoutineClientState) -> BTreeSet<String> {
    s.detail_order
        .iter()
        .cloned()
        .chain(s.starts.values().filter_map(|s| s.invocation_id.clone()))
        .collect()
}
fn bound_rows(
    rows: &mut BTreeMap<String, ClientRoutineInvocationSummary>,
    pins: &BTreeSet<String>,
) {
    let mut ids = rows
        .values()
        .filter(|r| !pins.contains(&r.invocation_id))
        .map(|r| (r.admission_sequence, r.invocation_id.clone()))
        .collect::<Vec<_>>();
    ids.sort_by(|a, b| b.cmp(a));
    for (_, id) in ids.into_iter().skip(MAX_RETAINED_ROUTINE_INVOCATIONS) {
        rows.remove(&id);
    }
}
fn bound(s: &mut ClientState) {
    sync_selected_pages(&mut s.routines);
    s.routines.selected_page_order.retain(|key| {
        s.routines
            .requests
            .get(key)
            .is_some_and(|r| !r.selected_invocations.is_empty())
    });
    while s
        .routines
        .requests
        .values()
        .map(|r| r.selected_invocations.len())
        .sum::<usize>()
        > 200
    {
        let Some(key) = s.routines.selected_page_order.pop_front() else {
            break;
        };
        if let Some(receipt) = s.routines.requests.get_mut(&key) {
            receipt.selected_invocations.clear();
            receipt.status = RoutineRequestStatus::Expired;
            receipt.error = Some("selected page evicted; request it explicitly again".into());
            receipt.next_cursor = None;
        }
    }
    let pins = pinned(&s.routines);
    bound_rows(&mut s.routines.invocations, &pins);
    s.routines
        .operations
        .retain(|_, op| s.routines.invocations.contains_key(&op.invocation_id));
    let mut selected = s
        .routines
        .details
        .values()
        .filter_map(|d| d.detail.as_ref())
        .flat_map(|d| d.operations.iter().map(|op| op.id.clone()))
        .collect::<BTreeSet<_>>();
    let mut counts = BTreeMap::new();
    for id in &selected {
        if let Some(op) = s.routines.operations.get(id) {
            *counts.entry(op.invocation_id.clone()).or_insert(0usize) += 1;
        }
    }
    let mut recent = s
        .routines
        .operations
        .values()
        .filter(|op| !selected.contains(&op.id))
        .collect::<Vec<_>>();
    recent.sort_by_key(|op| std::cmp::Reverse(op.cursor));
    for op in recent {
        let count = counts.entry(op.invocation_id.clone()).or_insert(0);
        if *count < 100 {
            *count += 1;
            selected.insert(op.id.clone());
        }
    }
    s.routines.operations.retain(|id, _| selected.contains(id));
}
fn sync_selected_pages(s: &mut RoutineClientState) {
    let mut latest = s.invocations.clone();
    for row in s
        .requests
        .values()
        .flat_map(|r| r.selected_invocations.iter())
    {
        if latest
            .get(&row.invocation_id)
            .is_none_or(|current| current.observation_revision < row.observation_revision)
        {
            latest.insert(row.invocation_id.clone(), row.clone());
        }
    }
    for row in s
        .requests
        .values_mut()
        .flat_map(|r| r.selected_invocations.iter_mut())
    {
        if let Some(current) = latest.get(&row.invocation_id) {
            row.clone_from(current);
        }
    }
    for (id, row) in &mut s.invocations {
        if let Some(current) = latest.get(id) {
            row.clone_from(current);
        }
    }
}
fn reset(s: &mut ClientState, source: Option<&str>, message: &str) -> ClientUpdate {
    if let Some(source) = source {
        let Some(h) = s.routines.session_histories.get_mut(source) else {
            return ClientUpdate::default();
        };
        h.through_sequence = None;
        h.snapshot_high_water = None;
        h.snapshot_complete = false;
    } else {
        s.routines.through_sequence = None;
        s.routines.snapshot_high_water = None;
        s.routines.snapshot_complete = false;
    }
    failed(s, source, false, message.into())
}
fn watch<'a>(
    s: &'a mut ClientState,
    source: Option<&str>,
    definitions: bool,
) -> Option<&'a mut WatchState> {
    if definitions {
        Some(&mut s.routines.definition_watch)
    } else if let Some(source) = source {
        s.routines
            .session_histories
            .get_mut(source)
            .map(|h| &mut h.watch)
    } else {
        Some(&mut s.routines.invocation_watch)
    }
}
fn failed(
    s: &mut ClientState,
    source: Option<&str>,
    definitions: bool,
    message: String,
) -> ClientUpdate {
    let allowed = s.permits_watch_maintenance();
    let Some(w) = watch(s, source, definitions) else {
        return ClientUpdate::default();
    };
    if w.interests == 0 {
        return ClientUpdate::default();
    }
    w.status = if allowed {
        OperationStatus::Failed
    } else {
        OperationStatus::Waiting
    };
    w.error = Some(message);
    w.open_id = None;
    let delay_ms = watch_reconnect_delay_ms(w.retry_attempt);
    w.retry_attempt = w.retry_attempt.saturating_add(1);
    if !allowed {
        return ClientUpdate::default();
    }
    let effect = if definitions {
        RoutineEffect::SleepDefinitionReconnect { delay_ms }
    } else if let Some(source) = source {
        RoutineEffect::SleepSessionInvocationReconnect {
            session_id: source.into(),
            delay_ms,
        }
    } else {
        RoutineEffect::SleepInvocationReconnect { delay_ms }
    };
    ClientUpdate::with_effect(ClientEffect::Routine(effect))
}
fn release(w: &mut WatchState) {
    w.interests = w.interests.saturating_sub(1);
    if w.interests == 0 {
        *w = WatchState::default();
    }
}
fn received(w: &mut WatchState) {
    w.status = OperationStatus::Received;
    w.retry_attempt = 0;
    w.error = None;
}
fn opened(w: &mut WatchState, id: u64) {
    if w.open_id == Some(id) {
        w.status = OperationStatus::Opened;
        w.open_id = None;
    }
}
pub fn maintain_definition_watch(s: &mut ClientState) -> ClientUpdate {
    maintain(s, None, true)
}
pub fn maintain_invocation_watch(s: &mut ClientState) -> ClientUpdate {
    maintain(s, None, false)
}
pub fn maintain_session_invocation_watch(s: &mut ClientState, source: &str) -> ClientUpdate {
    maintain(s, Some(source), false)
}
fn maintain(s: &mut ClientState, source: Option<&str>, definitions: bool) -> ClientUpdate {
    let Some(w) = watch(s, source, definitions) else {
        return ClientUpdate::default();
    };
    if w.interests == 0
        || matches!(
            w.status,
            OperationStatus::Loading | OperationStatus::Opened | OperationStatus::Received
        )
    {
        return ClientUpdate::default();
    }
    if !s.permits_watch_maintenance() {
        watch(s, source, definitions).unwrap().status = OperationStatus::Waiting;
        return ClientUpdate::default();
    }
    let Some(open_id) = s.next_watch_open_id() else {
        return ClientUpdate::default();
    };
    let w = watch(s, source, definitions).unwrap();
    w.status = OperationStatus::Loading;
    w.open_id = Some(open_id);
    w.error = None;
    let effect = if definitions {
        RoutineEffect::WatchDefinitions {
            open_id,
            request_id: format!("definition-{open_id}"),
        }
    } else {
        let after_sequence = if let Some(source) = source {
            s.routines.session_histories[source]
                .through_sequence
                .clone()
        } else {
            s.routines.through_sequence.clone()
        };
        RoutineEffect::WatchInvocations {
            open_id,
            request_id: format!("observation-{open_id}"),
            after_sequence,
            metadata_filters: source
                .map(|source| {
                    BTreeMap::from([(ROUTINE_SOURCE_SESSION_ID_METADATA_KEY.into(), source.into())])
                })
                .unwrap_or_default(),
            session_id: source.map(str::to_owned),
        }
    };
    ClientUpdate::with_effect(ClientEffect::Routine(effect))
}
