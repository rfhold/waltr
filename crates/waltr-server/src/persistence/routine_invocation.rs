use std::{collections::BTreeMap, error::Error, fmt};

use super::PersistencePool;
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, QueryBuilder, Sqlite};

const JSON_VERSION: u8 = 1;
const MAX_JSON_BYTES: usize = 65_536;
const MAX_TASKS: usize = 64;
const MAX_ERROR_CODE_BYTES: usize = 128;
const MAX_ERROR_MESSAGE_BYTES: usize = 4_096;
const MAX_RESULT_NAME_CHARS: usize = 256;
const MAX_PAGE_SIZE: u32 = 256;
pub(crate) const MAX_METADATA_ENTRIES: usize = 32;
pub(crate) const MAX_METADATA_KEY_BYTES: usize = 256;
pub(crate) const MAX_METADATA_VALUE_BYTES: usize = 1_024;
pub(crate) const SOURCE_SESSION_METADATA_KEY: &str = "waltr.routine.source_session_id";
const RECONCILE_BATCH_SIZE: usize = 32;
pub(super) const ROUTINE_EVENT_SEQUENCE_LOCK_KEY: i64 = 0x4B55_5249_4556_4E54;

/// Durable trigger category captured at admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RoutineTriggerKind {
    Manual,
    Event,
}

impl RoutineTriggerKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Event => "event",
        }
    }

    fn parse(value: &str) -> Result<Self, RoutineStoreError> {
        match value {
            "manual" => Ok(Self::Manual),
            "event" => Ok(Self::Event),
            _ => Err(invalid_stored_data()),
        }
    }
}

/// Durable aggregate lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoutineInvocationStatus {
    Admitted,
    Running,
    Succeeded,
    Failed,
    Interrupted,
}

impl RoutineInvocationStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    fn parse(value: &str) -> Result<Self, RoutineStoreError> {
        match value {
            "admitted" => Ok(Self::Admitted),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            _ => Err(invalid_stored_data()),
        }
    }

    const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Interrupted)
    }
}

/// Closed durable journal event vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoutineEventKind {
    InvocationAdmitted,
    InvocationStarted,
    TaskStarted,
    TaskSucceeded,
    TaskSkipped,
    TaskBlocked,
    TaskFailed,
    TaskInterrupted,
    InvocationSucceeded,
    InvocationFailed,
    InvocationInterrupted,
}

impl RoutineEventKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::InvocationAdmitted => "invocation_admitted",
            Self::InvocationStarted => "invocation_started",
            Self::TaskStarted => "task_started",
            Self::TaskSucceeded => "task_succeeded",
            Self::TaskSkipped => "task_skipped",
            Self::TaskBlocked => "task_blocked",
            Self::TaskFailed => "task_failed",
            Self::TaskInterrupted => "task_interrupted",
            Self::InvocationSucceeded => "invocation_succeeded",
            Self::InvocationFailed => "invocation_failed",
            Self::InvocationInterrupted => "invocation_interrupted",
        }
    }

    fn parse(value: &str) -> Result<Self, RoutineStoreError> {
        match value {
            "invocation_admitted" => Ok(Self::InvocationAdmitted),
            "invocation_started" => Ok(Self::InvocationStarted),
            "task_started" => Ok(Self::TaskStarted),
            "task_succeeded" => Ok(Self::TaskSucceeded),
            "task_skipped" => Ok(Self::TaskSkipped),
            "task_blocked" => Ok(Self::TaskBlocked),
            "task_failed" => Ok(Self::TaskFailed),
            "task_interrupted" => Ok(Self::TaskInterrupted),
            "invocation_succeeded" => Ok(Self::InvocationSucceeded),
            "invocation_failed" => Ok(Self::InvocationFailed),
            "invocation_interrupted" => Ok(Self::InvocationInterrupted),
            _ => Err(invalid_stored_data()),
        }
    }
}

/// Immutable declaration-order task identity captured when the invocation is admitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoutineTaskDescriptor {
    pub name: String,
    pub task_type: String,
}

/// One typed value persisted in journal payloads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub(crate) enum StoredRoutineValue {
    String(String),
    Boolean(bool),
}

/// One declaration-ordered task or invocation result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredRoutineResult {
    pub name: String,
    pub value: StoredRoutineValue,
}

/// Sanitized terminal or task failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutineFailure {
    pub code: String,
    pub message: String,
}

/// Complete immutable admission input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutineAdmission {
    pub invocation_id: String,
    pub execution_id: String,
    pub routine_name: String,
    pub routine_revision: String,
    pub trigger_name: String,
    pub trigger_kind: RoutineTriggerKind,
    pub correlation_id: String,
    pub metadata: BTreeMap<String, String>,
    pub source_cursor: Option<String>,
    pub declared_tasks: Vec<RoutineTaskDescriptor>,
    pub admitted_at_ms: i64,
}

/// Immutable metadata carried by the durable admission event for self-contained replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoutineAdmissionEvent {
    pub admission_sequence: i64,
    pub routine_name: String,
    pub routine_revision: String,
    pub trigger_name: String,
    pub trigger_kind: RoutineTriggerKind,
    pub correlation_id: String,
    pub metadata: BTreeMap<String, String>,
    pub source_cursor: Option<String>,
    pub declared_tasks: Vec<RoutineTaskDescriptor>,
    pub admitted_at_ms: i64,
}

/// Durable invocation aggregate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutineInvocationSummary {
    pub admission_sequence: i64,
    pub invocation_id: String,
    pub execution_id: String,
    pub routine_name: String,
    pub routine_revision: String,
    pub trigger_name: String,
    pub trigger_kind: RoutineTriggerKind,
    pub correlation_id: String,
    pub metadata: BTreeMap<String, String>,
    pub source_cursor: Option<String>,
    pub declared_tasks: Vec<RoutineTaskDescriptor>,
    pub status: RoutineInvocationStatus,
    pub admitted_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub updated_at_ms: i64,
    pub terminal_error: Option<RoutineFailure>,
    pub latest_sequence: i64,
}

/// One committed journal event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutineInvocationEvent {
    pub sequence: i64,
    pub invocation_id: String,
    pub kind: RoutineEventKind,
    pub task_name: Option<String>,
    pub occurred_at_ms: i64,
    pub error: Option<RoutineFailure>,
    pub values: Option<Vec<StoredRoutineResult>>,
    pub session_references: Option<BTreeMap<String, String>>,
    pub admission: Option<RoutineAdmissionEvent>,
}

/// Aggregate and complete journal read from one database snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutineInvocationDetail {
    pub summary: RoutineInvocationSummary,
    pub events: Vec<RoutineInvocationEvent>,
}

/// Task-only lifecycle operation. Task failure has a separate atomic invocation-failure method.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RoutineTaskTransition {
    Started {
        session_references: BTreeMap<String, String>,
    },
    Succeeded {
        values: Vec<StoredRoutineResult>,
        session_references: BTreeMap<String, String>,
    },
    Skipped,
    Blocked(RoutineFailure),
    Interrupted(RoutineFailure),
}

/// Sequence range currently retained by the append-only journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RoutineSequenceBounds {
    pub low: Option<i64>,
    pub high: Option<i64>,
}

/// Purpose-specific durable Routine invocation store.
#[derive(Clone)]
pub(crate) struct RoutineInvocationStore {
    pool: PersistencePool,
}

impl fmt::Debug for RoutineInvocationStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RoutineInvocationStore")
            .finish_non_exhaustive()
    }
}

/// Sanitized Routine persistence failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoutineStoreError {
    Invalid(&'static str),
    NotFound,
    Conflict,
    Persistence,
}

impl fmt::Display for RoutineStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::NotFound => formatter.write_str("routine invocation was not found"),
            Self::Conflict => formatter.write_str("routine invocation state changed"),
            Self::Persistence => formatter.write_str("routine invocation persistence failed"),
        }
    }
}

impl Error for RoutineStoreError {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionedTasks {
    version: u8,
    tasks: Vec<RoutineTaskDescriptor>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionedValues {
    version: u8,
    values: Vec<StoredRoutineResult>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionedAdmission {
    version: u8,
    admission: RoutineAdmissionEvent,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionedSessionReferences {
    version: u8,
    sessions: BTreeMap<String, String>,
}

struct NewEvent<'a> {
    kind: RoutineEventKind,
    task_name: Option<&'a str>,
    occurred_at_ms: i64,
    error: Option<&'a RoutineFailure>,
    values_json: Option<String>,
    sessions_json: Option<String>,
}

impl RoutineInvocationStore {
    pub(super) const fn new(pool: PersistencePool) -> Self {
        Self { pool }
    }

    /// Atomically create an aggregate and its admission event.
    pub(crate) async fn admit(
        &self,
        admission: RoutineAdmission,
    ) -> Result<(RoutineInvocationSummary, RoutineInvocationEvent), RoutineStoreError> {
        validate_admission(&admission)?;
        let tasks_json = encode_json(&VersionedTasks {
            version: JSON_VERSION,
            tasks: admission.declared_tasks.clone(),
        })?;
        match &self.pool {
            PersistencePool::Sqlite(pool) => {
                let mut tx = pool.begin().await.map_err(store_error)?;
                let admission_sequence = sqlx::query_scalar::<_, i64>("INSERT INTO routine_invocations (invocation_id, execution_id, routine_name, routine_revision, trigger_name, trigger_kind, correlation_id, source_cursor, declared_tasks_json, status, admitted_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'admitted', ?, ?) RETURNING admission_sequence")
                    .bind(&admission.invocation_id).bind(&admission.execution_id)
                    .bind(&admission.routine_name).bind(&admission.routine_revision)
                    .bind(&admission.trigger_name).bind(admission.trigger_kind.as_str())
                    .bind(&admission.correlation_id).bind(&admission.source_cursor).bind(&tasks_json)
                    .bind(admission.admitted_at_ms).bind(admission.admitted_at_ms)
                    .fetch_one(&mut *tx).await.map_err(|_| RoutineStoreError::Conflict)?;
                for (key, value) in &admission.metadata {
                    sqlx::query("INSERT INTO routine_invocation_metadata (invocation_id, metadata_key, metadata_value) VALUES (?, ?, ?)")
                        .bind(&admission.invocation_id).bind(key).bind(value)
                        .execute(&mut *tx).await.map_err(store_error)?;
                }
                let admission_event = admission_event(&admission, admission_sequence);
                let admission_json = encode_json(&VersionedAdmission {
                    version: JSON_VERSION,
                    admission: admission_event.clone(),
                })?;
                let sequence = sqlx::query_scalar::<_, i64>("INSERT INTO routine_invocation_events (invocation_id, event_kind, occurred_at_ms, values_json) VALUES (?, 'invocation_admitted', ?, ?) RETURNING sequence")
                    .bind(&admission.invocation_id).bind(admission.admitted_at_ms).bind(&admission_json)
                    .fetch_one(&mut *tx).await.map_err(store_error)?;
                sqlx::query(
                    "UPDATE routine_invocations SET latest_sequence = ? WHERE invocation_id = ?",
                )
                .bind(sequence)
                .bind(&admission.invocation_id)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
                tx.commit().await.map_err(store_error)?;
                Ok(admission_result(
                    admission,
                    admission_event,
                    admission_sequence,
                    sequence,
                ))
            }
            PersistencePool::PostgreSql(pool) => {
                let mut tx = pool.begin().await.map_err(store_error)?;
                let admission_sequence = sqlx::query_scalar::<_, i64>("INSERT INTO waltr.routine_invocations (invocation_id, execution_id, routine_name, routine_revision, trigger_name, trigger_kind, correlation_id, source_cursor, declared_tasks_json, status, admitted_at_ms, updated_at_ms) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'admitted', $10, $10) RETURNING admission_sequence")
                    .bind(&admission.invocation_id).bind(&admission.execution_id)
                    .bind(&admission.routine_name).bind(&admission.routine_revision)
                    .bind(&admission.trigger_name).bind(admission.trigger_kind.as_str())
                    .bind(&admission.correlation_id).bind(&admission.source_cursor)
                    .bind(&tasks_json).bind(admission.admitted_at_ms)
                    .fetch_one(&mut *tx).await.map_err(|_| RoutineStoreError::Conflict)?;
                for (key, value) in &admission.metadata {
                    sqlx::query("INSERT INTO waltr.routine_invocation_metadata (invocation_id, metadata_key, metadata_value) VALUES ($1, $2, $3)")
                        .bind(&admission.invocation_id).bind(key).bind(value)
                        .execute(&mut *tx).await.map_err(store_error)?;
                }
                let admission_event = admission_event(&admission, admission_sequence);
                let admission_json = encode_json(&VersionedAdmission {
                    version: JSON_VERSION,
                    admission: admission_event.clone(),
                })?;
                let sequence = insert_event_postgres(
                    &mut tx,
                    &admission.invocation_id,
                    &NewEvent {
                        kind: RoutineEventKind::InvocationAdmitted,
                        task_name: None,
                        occurred_at_ms: admission.admitted_at_ms,
                        error: None,
                        values_json: Some(admission_json),
                        sessions_json: None,
                    },
                )
                .await?;
                sqlx::query("UPDATE waltr.routine_invocations SET latest_sequence = $1 WHERE invocation_id = $2")
                    .bind(sequence).bind(&admission.invocation_id).execute(&mut *tx).await.map_err(store_error)?;
                tx.commit().await.map_err(store_error)?;
                Ok(admission_result(
                    admission,
                    admission_event,
                    admission_sequence,
                    sequence,
                ))
            }
        }
    }

    pub(crate) async fn start_invocation(
        &self,
        invocation_id: &str,
        occurred_at_ms: i64,
    ) -> Result<RoutineInvocationEvent, RoutineStoreError> {
        let summary = self.required(invocation_id).await?;
        if summary.status != RoutineInvocationStatus::Admitted {
            return Err(RoutineStoreError::Conflict);
        }
        self.append(
            &summary,
            NewEvent {
                kind: RoutineEventKind::InvocationStarted,
                task_name: None,
                occurred_at_ms,
                error: None,
                values_json: None,
                sessions_json: None,
            },
            RoutineInvocationStatus::Running,
            false,
        )
        .await
    }

    pub(crate) async fn transition_task(
        &self,
        invocation_id: &str,
        task_name: &str,
        transition: RoutineTaskTransition,
        occurred_at_ms: i64,
    ) -> Result<RoutineInvocationEvent, RoutineStoreError> {
        let summary = self.required(invocation_id).await?;
        if summary.status != RoutineInvocationStatus::Running
            || !summary
                .declared_tasks
                .iter()
                .any(|task| task.name == task_name)
        {
            return Err(RoutineStoreError::Conflict);
        }
        let task_state = task_state(&self.events_for_invocation(invocation_id).await?, task_name);
        let (kind, error, values, sessions) = match &transition {
            RoutineTaskTransition::Started { session_references } if task_state.is_none() => (
                RoutineEventKind::TaskStarted,
                None,
                None,
                encode_sessions(session_references)?,
            ),
            RoutineTaskTransition::Succeeded {
                values,
                session_references,
            } if task_state == Some(TaskState::Started) => (
                RoutineEventKind::TaskSucceeded,
                None,
                encode_values(values)?,
                encode_sessions(session_references)?,
            ),
            RoutineTaskTransition::Skipped if task_state.is_none() => {
                (RoutineEventKind::TaskSkipped, None, None, None)
            }
            RoutineTaskTransition::Blocked(failure) if task_state.is_none() => {
                (RoutineEventKind::TaskBlocked, Some(failure), None, None)
            }
            RoutineTaskTransition::Interrupted(failure)
                if task_state == Some(TaskState::Started) =>
            {
                (RoutineEventKind::TaskInterrupted, Some(failure), None, None)
            }
            _ => return Err(RoutineStoreError::Conflict),
        };
        if let Some(error) = error {
            validate_failure(error)?;
        }
        self.append(
            &summary,
            NewEvent {
                kind,
                task_name: Some(task_name),
                occurred_at_ms,
                error,
                values_json: values,
                sessions_json: sessions,
            },
            RoutineInvocationStatus::Running,
            false,
        )
        .await
    }

    /// Atomically append task_failed and invocation_failed and commit one terminal aggregate CAS.
    pub(crate) async fn fail_task_and_invocation(
        &self,
        invocation_id: &str,
        task_name: &str,
        failure: RoutineFailure,
        occurred_at_ms: i64,
    ) -> Result<[RoutineInvocationEvent; 2], RoutineStoreError> {
        validate_failure(&failure)?;
        let summary = self.required(invocation_id).await?;
        if summary.status != RoutineInvocationStatus::Running
            || !summary
                .declared_tasks
                .iter()
                .any(|task| task.name == task_name)
            || task_state(&self.events_for_invocation(invocation_id).await?, task_name)
                != Some(TaskState::Started)
        {
            return Err(RoutineStoreError::Conflict);
        }
        self.append_task_failure(&summary, task_name, &failure, occurred_at_ms)
            .await
    }

    pub(crate) async fn finish_success(
        &self,
        invocation_id: &str,
        values: Vec<StoredRoutineResult>,
        occurred_at_ms: i64,
    ) -> Result<RoutineInvocationEvent, RoutineStoreError> {
        let summary = self.required(invocation_id).await?;
        let events = self.events_for_invocation(invocation_id).await?;
        if summary.status != RoutineInvocationStatus::Running
            || summary.declared_tasks.iter().any(|task| {
                !matches!(
                    task_state(&events, &task.name),
                    Some(TaskState::Succeeded | TaskState::Skipped)
                )
            })
        {
            return Err(RoutineStoreError::Conflict);
        }
        self.append(
            &summary,
            NewEvent {
                kind: RoutineEventKind::InvocationSucceeded,
                task_name: None,
                occurred_at_ms,
                error: None,
                values_json: encode_values(&values)?,
                sessions_json: None,
            },
            RoutineInvocationStatus::Succeeded,
            true,
        )
        .await
    }

    pub(crate) async fn finish_failure(
        &self,
        invocation_id: &str,
        failure: RoutineFailure,
        occurred_at_ms: i64,
    ) -> Result<RoutineInvocationEvent, RoutineStoreError> {
        self.finish_with_failure(
            invocation_id,
            failure,
            occurred_at_ms,
            RoutineEventKind::InvocationFailed,
            RoutineInvocationStatus::Failed,
        )
        .await
    }

    pub(crate) async fn finish_interruption(
        &self,
        invocation_id: &str,
        failure: RoutineFailure,
        occurred_at_ms: i64,
    ) -> Result<Vec<RoutineInvocationEvent>, RoutineStoreError> {
        validate_failure(&failure)?;
        let summary = self.required(invocation_id).await?;
        if summary.status.is_terminal() {
            return Err(RoutineStoreError::Conflict);
        }
        let events = self.events_for_invocation(invocation_id).await?;
        let unfinished_tasks = summary
            .declared_tasks
            .iter()
            .filter(|task| {
                matches!(
                    task_state(&events, &task.name),
                    None | Some(TaskState::Started)
                )
            })
            .map(|task| task.name.as_str())
            .collect::<Vec<_>>();
        self.append_interruption(&summary, &unfinished_tasks, &failure, occurred_at_ms)
            .await
    }

    async fn finish_with_failure(
        &self,
        invocation_id: &str,
        failure: RoutineFailure,
        occurred_at_ms: i64,
        kind: RoutineEventKind,
        status: RoutineInvocationStatus,
    ) -> Result<RoutineInvocationEvent, RoutineStoreError> {
        validate_failure(&failure)?;
        let summary = self.required(invocation_id).await?;
        if summary.status.is_terminal() {
            return Err(RoutineStoreError::Conflict);
        }
        self.append(
            &summary,
            NewEvent {
                kind,
                task_name: None,
                occurred_at_ms,
                error: Some(&failure),
                values_json: None,
                sessions_json: None,
            },
            status,
            true,
        )
        .await
    }

    /// Interrupt every invocation observed as unfinished. Concurrently changed records are skipped.
    pub(crate) async fn reconcile_unfinished(
        &self,
        failure: RoutineFailure,
        occurred_at_ms: i64,
    ) -> Result<Vec<RoutineInvocationEvent>, RoutineStoreError> {
        validate_failure(&failure)?;
        let mut committed = Vec::new();
        loop {
            let batch = self.nonterminal(RECONCILE_BATCH_SIZE).await?;
            if batch.is_empty() {
                break;
            }
            for summary in batch {
                match self
                    .finish_interruption(&summary.invocation_id, failure.clone(), occurred_at_ms)
                    .await
                {
                    Ok(events) => committed.extend(events),
                    Err(RoutineStoreError::Conflict | RoutineStoreError::NotFound) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(committed)
    }

    pub(crate) async fn get(
        &self,
        invocation_id: &str,
    ) -> Result<Option<RoutineInvocationSummary>, RoutineStoreError> {
        let row = match &self.pool {
            PersistencePool::Sqlite(pool) => sqlx::query(SUMMARY_SQLITE)
                .bind(invocation_id)
                .fetch_optional(pool)
                .await
                .map_err(store_error)?
                .as_ref()
                .map(parse_summary)
                .transpose()?,
            PersistencePool::PostgreSql(pool) => sqlx::query(SUMMARY_POSTGRES)
                .bind(invocation_id)
                .fetch_optional(pool)
                .await
                .map_err(store_error)?
                .as_ref()
                .map(parse_summary)
                .transpose()?,
        };
        Ok(row)
    }

    /// Read one aggregate and its complete journal through `latest_sequence` from one snapshot.
    pub(crate) async fn detail(
        &self,
        invocation_id: &str,
    ) -> Result<RoutineInvocationDetail, RoutineStoreError> {
        match &self.pool {
            PersistencePool::Sqlite(pool) => {
                let mut tx = pool.begin().await.map_err(store_error)?;
                let row = sqlx::query(SUMMARY_SQLITE)
                    .bind(invocation_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(store_error)?
                    .ok_or(RoutineStoreError::NotFound)?;
                let summary = parse_summary(&row)?;
                let rows = sqlx::query(&format!("{EVENT_COLUMNS} FROM routine_invocation_events WHERE invocation_id = ? AND sequence <= ? ORDER BY sequence ASC"))
                    .bind(invocation_id).bind(summary.latest_sequence)
                    .fetch_all(&mut *tx).await.map_err(store_error)?;
                let events = rows
                    .iter()
                    .map(parse_event)
                    .collect::<Result<Vec<_>, _>>()?;
                validate_detail_journal(&summary, &events)?;
                tx.commit().await.map_err(store_error)?;
                Ok(RoutineInvocationDetail { summary, events })
            }
            PersistencePool::PostgreSql(pool) => {
                let mut tx = pool.begin().await.map_err(store_error)?;
                sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                    .execute(&mut *tx)
                    .await
                    .map_err(store_error)?;
                let row = sqlx::query(SUMMARY_POSTGRES)
                    .bind(invocation_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(store_error)?
                    .ok_or(RoutineStoreError::NotFound)?;
                let summary = parse_summary(&row)?;
                let rows = sqlx::query(&format!("{EVENT_COLUMNS} FROM waltr.routine_invocation_events WHERE invocation_id = $1 AND sequence <= $2 ORDER BY sequence ASC"))
                    .bind(invocation_id).bind(summary.latest_sequence)
                    .fetch_all(&mut *tx).await.map_err(store_error)?;
                let events = rows
                    .iter()
                    .map(parse_event)
                    .collect::<Result<Vec<_>, _>>()?;
                validate_detail_journal(&summary, &events)?;
                tx.commit().await.map_err(store_error)?;
                Ok(RoutineInvocationDetail { summary, events })
            }
        }
    }

    /// List newest admissions first. `before` is an exclusive admission-sequence cursor.
    pub(crate) async fn list(
        &self,
        before: Option<i64>,
        limit: u32,
        metadata_filters: &BTreeMap<String, String>,
    ) -> Result<Vec<RoutineInvocationSummary>, RoutineStoreError> {
        validate_metadata(metadata_filters)?;
        let limit = i64::from(limit.clamp(1, MAX_PAGE_SIZE));
        match &self.pool {
            PersistencePool::Sqlite(pool) => {
                let mut query = QueryBuilder::<Sqlite>::new(SUMMARY_SELECT_SQLITE);
                query
                    .push(" WHERE (")
                    .push_bind(before)
                    .push(" IS NULL OR i.admission_sequence < ")
                    .push_bind(before)
                    .push(")");
                push_metadata_filters(
                    &mut query,
                    metadata_filters,
                    "i.invocation_id",
                    "routine_invocation_metadata",
                );
                query
                    .push(" ORDER BY i.admission_sequence DESC LIMIT ")
                    .push_bind(limit);
                query
                    .build()
                    .fetch_all(pool)
                    .await
                    .map_err(store_error)?
                    .iter()
                    .map(parse_summary)
                    .collect()
            }
            PersistencePool::PostgreSql(pool) => {
                let mut query = QueryBuilder::<Postgres>::new(SUMMARY_SELECT_POSTGRES);
                query
                    .push(" WHERE (")
                    .push_bind(before)
                    .push(" IS NULL OR i.admission_sequence < ")
                    .push_bind(before)
                    .push(")");
                push_metadata_filters(
                    &mut query,
                    metadata_filters,
                    "i.invocation_id",
                    "waltr.routine_invocation_metadata",
                );
                query
                    .push(" ORDER BY i.admission_sequence DESC LIMIT ")
                    .push_bind(limit);
                query
                    .build()
                    .fetch_all(pool)
                    .await
                    .map_err(store_error)?
                    .iter()
                    .map(parse_summary)
                    .collect()
            }
        }
    }

    pub(crate) async fn events_after(
        &self,
        cursor: i64,
        limit: u32,
        metadata_filters: &BTreeMap<String, String>,
    ) -> Result<Vec<RoutineInvocationEvent>, RoutineStoreError> {
        validate_metadata(metadata_filters)?;
        let limit = i64::from(limit.clamp(1, MAX_PAGE_SIZE));
        match &self.pool {
            PersistencePool::Sqlite(pool) => {
                let mut query = QueryBuilder::<Sqlite>::new(format!(
                    "{EVENT_COLUMNS} FROM routine_invocation_events AS e WHERE e.sequence > "
                ));
                query.push_bind(cursor);
                push_metadata_filters(
                    &mut query,
                    metadata_filters,
                    "e.invocation_id",
                    "routine_invocation_metadata",
                );
                query
                    .push(" ORDER BY e.sequence ASC LIMIT ")
                    .push_bind(limit);
                query
                    .build()
                    .fetch_all(pool)
                    .await
                    .map_err(store_error)?
                    .iter()
                    .map(parse_event)
                    .collect()
            }
            PersistencePool::PostgreSql(pool) => {
                let mut query = QueryBuilder::<Postgres>::new(format!(
                    "{EVENT_COLUMNS} FROM waltr.routine_invocation_events AS e WHERE e.sequence > "
                ));
                query.push_bind(cursor);
                push_metadata_filters(
                    &mut query,
                    metadata_filters,
                    "e.invocation_id",
                    "waltr.routine_invocation_metadata",
                );
                query
                    .push(" ORDER BY e.sequence ASC LIMIT ")
                    .push_bind(limit);
                query
                    .build()
                    .fetch_all(pool)
                    .await
                    .map_err(store_error)?
                    .iter()
                    .map(parse_event)
                    .collect()
            }
        }
    }

    pub(crate) async fn events_for_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Vec<RoutineInvocationEvent>, RoutineStoreError> {
        match &self.pool {
            PersistencePool::Sqlite(pool) => sqlx::query(&format!("{EVENT_COLUMNS} FROM routine_invocation_events WHERE invocation_id = ? ORDER BY sequence ASC"))
                .bind(invocation_id).fetch_all(pool).await.map_err(store_error)?.iter().map(parse_event).collect(),
            PersistencePool::PostgreSql(pool) => sqlx::query(&format!("{EVENT_COLUMNS} FROM waltr.routine_invocation_events WHERE invocation_id = $1 ORDER BY sequence ASC"))
                .bind(invocation_id).fetch_all(pool).await.map_err(store_error)?.iter().map(parse_event).collect(),
        }
    }

    pub(crate) async fn nonterminal(
        &self,
        limit: usize,
    ) -> Result<Vec<RoutineInvocationSummary>, RoutineStoreError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        match &self.pool {
            PersistencePool::Sqlite(pool) => sqlx::query(NONTERMINAL_SQLITE)
                .bind(limit)
                .fetch_all(pool)
                .await
                .map_err(store_error)?
                .iter()
                .map(parse_summary)
                .collect(),
            PersistencePool::PostgreSql(pool) => sqlx::query(NONTERMINAL_POSTGRES)
                .bind(limit)
                .fetch_all(pool)
                .await
                .map_err(store_error)?
                .iter()
                .map(parse_summary)
                .collect(),
        }
    }

    pub(crate) async fn sequence_bounds(&self) -> Result<RoutineSequenceBounds, RoutineStoreError> {
        let (low, high) = match &self.pool {
            PersistencePool::Sqlite(pool) => sqlx::query_as::<_, (Option<i64>, Option<i64>)>(
                "SELECT MIN(sequence), MAX(sequence) FROM routine_invocation_events",
            )
            .fetch_one(pool)
            .await
            .map_err(store_error)?,
            PersistencePool::PostgreSql(pool) => sqlx::query_as::<_, (Option<i64>, Option<i64>)>(
                "SELECT MIN(sequence), MAX(sequence) FROM waltr.routine_invocation_events",
            )
            .fetch_one(pool)
            .await
            .map_err(store_error)?,
        };
        Ok(RoutineSequenceBounds { low, high })
    }

    pub(crate) async fn high_water(&self) -> Result<Option<i64>, RoutineStoreError> {
        self.sequence_bounds().await.map(|bounds| bounds.high)
    }

    async fn required(
        &self,
        invocation_id: &str,
    ) -> Result<RoutineInvocationSummary, RoutineStoreError> {
        self.get(invocation_id)
            .await?
            .ok_or(RoutineStoreError::NotFound)
    }

    async fn append(
        &self,
        summary: &RoutineInvocationSummary,
        event: NewEvent<'_>,
        status: RoutineInvocationStatus,
        terminal: bool,
    ) -> Result<RoutineInvocationEvent, RoutineStoreError> {
        match &self.pool {
            PersistencePool::Sqlite(pool) => {
                let mut tx = pool.begin().await.map_err(store_error)?;
                let sequence = insert_event_sqlite(&mut tx, &summary.invocation_id, &event).await?;
                let affected = sqlx::query("UPDATE routine_invocations SET status = ?, started_at_ms = CASE WHEN ? = 'invocation_started' THEN ? ELSE started_at_ms END, completed_at_ms = CASE WHEN ? THEN ? ELSE completed_at_ms END, updated_at_ms = ?, terminal_error_code = CASE WHEN ? THEN ? ELSE terminal_error_code END, terminal_error_message = CASE WHEN ? THEN ? ELSE terminal_error_message END, latest_sequence = ? WHERE invocation_id = ? AND status = ? AND latest_sequence = ?")
                    .bind(status.as_str()).bind(event.kind.as_str()).bind(event.occurred_at_ms)
                    .bind(terminal).bind(event.occurred_at_ms).bind(event.occurred_at_ms)
                    .bind(terminal).bind(event.error.map(|error| &error.code))
                    .bind(terminal).bind(event.error.map(|error| &error.message))
                    .bind(sequence).bind(&summary.invocation_id).bind(summary.status.as_str()).bind(summary.latest_sequence)
                    .execute(&mut *tx).await.map_err(store_error)?.rows_affected();
                if affected != 1 {
                    return Err(RoutineStoreError::Conflict);
                }
                tx.commit().await.map_err(store_error)?;
                Ok(committed_event(sequence, &summary.invocation_id, event)?)
            }
            PersistencePool::PostgreSql(pool) => {
                let mut tx = pool.begin().await.map_err(store_error)?;
                let sequence =
                    insert_event_postgres(&mut tx, &summary.invocation_id, &event).await?;
                let affected = sqlx::query("UPDATE waltr.routine_invocations SET status = $1, started_at_ms = CASE WHEN $2 = 'invocation_started' THEN $3 ELSE started_at_ms END, completed_at_ms = CASE WHEN $4 THEN $3 ELSE completed_at_ms END, updated_at_ms = $3, terminal_error_code = CASE WHEN $4 THEN $5 ELSE terminal_error_code END, terminal_error_message = CASE WHEN $4 THEN $6 ELSE terminal_error_message END, latest_sequence = $7 WHERE invocation_id = $8 AND status = $9 AND latest_sequence = $10")
                    .bind(status.as_str()).bind(event.kind.as_str()).bind(event.occurred_at_ms)
                    .bind(terminal).bind(event.error.map(|error| &error.code)).bind(event.error.map(|error| &error.message))
                    .bind(sequence).bind(&summary.invocation_id).bind(summary.status.as_str()).bind(summary.latest_sequence)
                    .execute(&mut *tx).await.map_err(store_error)?.rows_affected();
                if affected != 1 {
                    return Err(RoutineStoreError::Conflict);
                }
                tx.commit().await.map_err(store_error)?;
                Ok(committed_event(sequence, &summary.invocation_id, event)?)
            }
        }
    }

    async fn append_task_failure(
        &self,
        summary: &RoutineInvocationSummary,
        task_name: &str,
        failure: &RoutineFailure,
        occurred_at_ms: i64,
    ) -> Result<[RoutineInvocationEvent; 2], RoutineStoreError> {
        let task = NewEvent {
            kind: RoutineEventKind::TaskFailed,
            task_name: Some(task_name),
            occurred_at_ms,
            error: Some(failure),
            values_json: None,
            sessions_json: None,
        };
        let invocation = NewEvent {
            kind: RoutineEventKind::InvocationFailed,
            task_name: None,
            occurred_at_ms,
            error: Some(failure),
            values_json: None,
            sessions_json: None,
        };
        let (task_sequence, invocation_sequence) = match &self.pool {
            PersistencePool::Sqlite(pool) => {
                let mut tx = pool.begin().await.map_err(store_error)?;
                let first = insert_event_sqlite(&mut tx, &summary.invocation_id, &task).await?;
                let second =
                    insert_event_sqlite(&mut tx, &summary.invocation_id, &invocation).await?;
                let affected = sqlx::query("UPDATE routine_invocations SET status = 'failed', completed_at_ms = ?, updated_at_ms = ?, terminal_error_code = ?, terminal_error_message = ?, latest_sequence = ? WHERE invocation_id = ? AND status = 'running' AND latest_sequence = ?")
                    .bind(occurred_at_ms).bind(occurred_at_ms).bind(&failure.code).bind(&failure.message).bind(second).bind(&summary.invocation_id).bind(summary.latest_sequence)
                    .execute(&mut *tx).await.map_err(store_error)?.rows_affected();
                if affected != 1 {
                    return Err(RoutineStoreError::Conflict);
                }
                tx.commit().await.map_err(store_error)?;
                (first, second)
            }
            PersistencePool::PostgreSql(pool) => {
                let mut tx = pool.begin().await.map_err(store_error)?;
                let first = insert_event_postgres(&mut tx, &summary.invocation_id, &task).await?;
                let second =
                    insert_event_postgres(&mut tx, &summary.invocation_id, &invocation).await?;
                let affected = sqlx::query("UPDATE waltr.routine_invocations SET status = 'failed', completed_at_ms = $1, updated_at_ms = $1, terminal_error_code = $2, terminal_error_message = $3, latest_sequence = $4 WHERE invocation_id = $5 AND status = 'running' AND latest_sequence = $6")
                    .bind(occurred_at_ms).bind(&failure.code).bind(&failure.message).bind(second).bind(&summary.invocation_id).bind(summary.latest_sequence)
                    .execute(&mut *tx).await.map_err(store_error)?.rows_affected();
                if affected != 1 {
                    return Err(RoutineStoreError::Conflict);
                }
                tx.commit().await.map_err(store_error)?;
                (first, second)
            }
        };
        Ok([
            committed_event(task_sequence, &summary.invocation_id, task)?,
            committed_event(invocation_sequence, &summary.invocation_id, invocation)?,
        ])
    }

    async fn append_interruption(
        &self,
        summary: &RoutineInvocationSummary,
        unfinished_tasks: &[&str],
        failure: &RoutineFailure,
        occurred_at_ms: i64,
    ) -> Result<Vec<RoutineInvocationEvent>, RoutineStoreError> {
        let task_events = unfinished_tasks
            .iter()
            .map(|task_name| NewEvent {
                kind: RoutineEventKind::TaskInterrupted,
                task_name: Some(*task_name),
                occurred_at_ms,
                error: Some(failure),
                values_json: None,
                sessions_json: None,
            })
            .collect::<Vec<_>>();
        let invocation_event = NewEvent {
            kind: RoutineEventKind::InvocationInterrupted,
            task_name: None,
            occurred_at_ms,
            error: Some(failure),
            values_json: None,
            sessions_json: None,
        };
        let sequences = match &self.pool {
            PersistencePool::Sqlite(pool) => {
                let mut tx = pool.begin().await.map_err(store_error)?;
                let mut sequences = Vec::with_capacity(task_events.len() + 1);
                for event in &task_events {
                    sequences
                        .push(insert_event_sqlite(&mut tx, &summary.invocation_id, event).await?);
                }
                let invocation_sequence =
                    insert_event_sqlite(&mut tx, &summary.invocation_id, &invocation_event).await?;
                sequences.push(invocation_sequence);
                let affected = sqlx::query("UPDATE routine_invocations SET status = 'interrupted', completed_at_ms = ?, updated_at_ms = ?, terminal_error_code = ?, terminal_error_message = ?, latest_sequence = ? WHERE invocation_id = ? AND status = ? AND latest_sequence = ?")
                    .bind(occurred_at_ms).bind(occurred_at_ms).bind(&failure.code).bind(&failure.message)
                    .bind(invocation_sequence).bind(&summary.invocation_id).bind(summary.status.as_str()).bind(summary.latest_sequence)
                    .execute(&mut *tx).await.map_err(store_error)?.rows_affected();
                if affected != 1 {
                    return Err(RoutineStoreError::Conflict);
                }
                tx.commit().await.map_err(store_error)?;
                sequences
            }
            PersistencePool::PostgreSql(pool) => {
                let mut tx = pool.begin().await.map_err(store_error)?;
                let mut sequences = Vec::with_capacity(task_events.len() + 1);
                for event in &task_events {
                    sequences
                        .push(insert_event_postgres(&mut tx, &summary.invocation_id, event).await?);
                }
                let invocation_sequence =
                    insert_event_postgres(&mut tx, &summary.invocation_id, &invocation_event)
                        .await?;
                sequences.push(invocation_sequence);
                let affected = sqlx::query("UPDATE waltr.routine_invocations SET status = 'interrupted', completed_at_ms = $1, updated_at_ms = $1, terminal_error_code = $2, terminal_error_message = $3, latest_sequence = $4 WHERE invocation_id = $5 AND status = $6 AND latest_sequence = $7")
                    .bind(occurred_at_ms).bind(&failure.code).bind(&failure.message)
                    .bind(invocation_sequence).bind(&summary.invocation_id).bind(summary.status.as_str()).bind(summary.latest_sequence)
                    .execute(&mut *tx).await.map_err(store_error)?.rows_affected();
                if affected != 1 {
                    return Err(RoutineStoreError::Conflict);
                }
                tx.commit().await.map_err(store_error)?;
                sequences
            }
        };
        task_events
            .into_iter()
            .chain(std::iter::once(invocation_event))
            .zip(sequences)
            .map(|(event, sequence)| committed_event(sequence, &summary.invocation_id, event))
            .collect()
    }
}

async fn insert_event_sqlite(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    invocation_id: &str,
    event: &NewEvent<'_>,
) -> Result<i64, RoutineStoreError> {
    sqlx::query_scalar("INSERT INTO routine_invocation_events (invocation_id, event_kind, task_name, occurred_at_ms, error_code, error_message, values_json, session_references_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING sequence")
        .bind(invocation_id).bind(event.kind.as_str()).bind(event.task_name).bind(event.occurred_at_ms)
        .bind(event.error.map(|error| &error.code)).bind(event.error.map(|error| &error.message))
        .bind(&event.values_json).bind(&event.sessions_json).fetch_one(&mut **tx).await.map_err(store_error)
}

async fn insert_event_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    invocation_id: &str,
    event: &NewEvent<'_>,
) -> Result<i64, RoutineStoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ROUTINE_EVENT_SEQUENCE_LOCK_KEY)
        .execute(&mut **tx)
        .await
        .map_err(store_error)?;
    sqlx::query_scalar("INSERT INTO waltr.routine_invocation_events (invocation_id, event_kind, task_name, occurred_at_ms, error_code, error_message, values_json, session_references_json) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING sequence")
        .bind(invocation_id).bind(event.kind.as_str()).bind(event.task_name).bind(event.occurred_at_ms)
        .bind(event.error.map(|error| &error.code)).bind(event.error.map(|error| &error.message))
        .bind(&event.values_json).bind(&event.sessions_json).fetch_one(&mut **tx).await.map_err(store_error)
}

const SUMMARY_SELECT_SQLITE: &str = "SELECT i.admission_sequence, i.invocation_id, i.execution_id, i.routine_name, i.routine_revision, i.trigger_name, i.trigger_kind, i.correlation_id, i.source_cursor, i.declared_tasks_json, i.status, i.admitted_at_ms, i.started_at_ms, i.completed_at_ms, i.updated_at_ms, i.terminal_error_code, i.terminal_error_message, i.latest_sequence, COALESCE((SELECT json_group_object(m.metadata_key, m.metadata_value) FROM routine_invocation_metadata AS m WHERE m.invocation_id = i.invocation_id), '{}') AS metadata_json FROM routine_invocations AS i";
const SUMMARY_SELECT_POSTGRES: &str = "SELECT i.admission_sequence, i.invocation_id, i.execution_id, i.routine_name, i.routine_revision, i.trigger_name, i.trigger_kind, i.correlation_id, i.source_cursor, i.declared_tasks_json, i.status, i.admitted_at_ms, i.started_at_ms, i.completed_at_ms, i.updated_at_ms, i.terminal_error_code, i.terminal_error_message, i.latest_sequence, COALESCE((SELECT json_object_agg(m.metadata_key, m.metadata_value)::text FROM waltr.routine_invocation_metadata AS m WHERE m.invocation_id = i.invocation_id), '{}') AS metadata_json FROM waltr.routine_invocations AS i";
const SUMMARY_SQLITE: &str = "SELECT i.admission_sequence, i.invocation_id, i.execution_id, i.routine_name, i.routine_revision, i.trigger_name, i.trigger_kind, i.correlation_id, i.source_cursor, i.declared_tasks_json, i.status, i.admitted_at_ms, i.started_at_ms, i.completed_at_ms, i.updated_at_ms, i.terminal_error_code, i.terminal_error_message, i.latest_sequence, COALESCE((SELECT json_group_object(m.metadata_key, m.metadata_value) FROM routine_invocation_metadata AS m WHERE m.invocation_id = i.invocation_id), '{}') AS metadata_json FROM routine_invocations AS i WHERE i.invocation_id = ?";
const SUMMARY_POSTGRES: &str = "SELECT i.admission_sequence, i.invocation_id, i.execution_id, i.routine_name, i.routine_revision, i.trigger_name, i.trigger_kind, i.correlation_id, i.source_cursor, i.declared_tasks_json, i.status, i.admitted_at_ms, i.started_at_ms, i.completed_at_ms, i.updated_at_ms, i.terminal_error_code, i.terminal_error_message, i.latest_sequence, COALESCE((SELECT json_object_agg(m.metadata_key, m.metadata_value)::text FROM waltr.routine_invocation_metadata AS m WHERE m.invocation_id = i.invocation_id), '{}') AS metadata_json FROM waltr.routine_invocations AS i WHERE i.invocation_id = $1";
const NONTERMINAL_SQLITE: &str = "SELECT i.admission_sequence, i.invocation_id, i.execution_id, i.routine_name, i.routine_revision, i.trigger_name, i.trigger_kind, i.correlation_id, i.source_cursor, i.declared_tasks_json, i.status, i.admitted_at_ms, i.started_at_ms, i.completed_at_ms, i.updated_at_ms, i.terminal_error_code, i.terminal_error_message, i.latest_sequence, COALESCE((SELECT json_group_object(m.metadata_key, m.metadata_value) FROM routine_invocation_metadata AS m WHERE m.invocation_id = i.invocation_id), '{}') AS metadata_json FROM routine_invocations AS i WHERE i.status IN ('admitted', 'running') ORDER BY i.admission_sequence ASC LIMIT ?";
const NONTERMINAL_POSTGRES: &str = "SELECT i.admission_sequence, i.invocation_id, i.execution_id, i.routine_name, i.routine_revision, i.trigger_name, i.trigger_kind, i.correlation_id, i.source_cursor, i.declared_tasks_json, i.status, i.admitted_at_ms, i.started_at_ms, i.completed_at_ms, i.updated_at_ms, i.terminal_error_code, i.terminal_error_message, i.latest_sequence, COALESCE((SELECT json_object_agg(m.metadata_key, m.metadata_value)::text FROM waltr.routine_invocation_metadata AS m WHERE m.invocation_id = i.invocation_id), '{}') AS metadata_json FROM waltr.routine_invocations AS i WHERE i.status IN ('admitted', 'running') ORDER BY i.admission_sequence ASC LIMIT $1";
const EVENT_COLUMNS: &str = "SELECT sequence, invocation_id, event_kind, task_name, occurred_at_ms, error_code, error_message, values_json, session_references_json";

fn admission_event(admission: &RoutineAdmission, admission_sequence: i64) -> RoutineAdmissionEvent {
    RoutineAdmissionEvent {
        admission_sequence,
        routine_name: admission.routine_name.clone(),
        routine_revision: admission.routine_revision.clone(),
        trigger_name: admission.trigger_name.clone(),
        trigger_kind: admission.trigger_kind,
        correlation_id: admission.correlation_id.clone(),
        metadata: admission.metadata.clone(),
        source_cursor: admission.source_cursor.clone(),
        declared_tasks: admission.declared_tasks.clone(),
        admitted_at_ms: admission.admitted_at_ms,
    }
}

fn validate_detail_journal(
    summary: &RoutineInvocationSummary,
    events: &[RoutineInvocationEvent],
) -> Result<(), RoutineStoreError> {
    if events.last().map(|event| event.sequence) != Some(summary.latest_sequence)
        || events
            .first()
            .and_then(|event| event.admission.as_ref())
            .map(|admission| admission.admission_sequence)
            != Some(summary.admission_sequence)
    {
        return Err(invalid_stored_data());
    }
    Ok(())
}

fn admission_result(
    admission: RoutineAdmission,
    admission_event: RoutineAdmissionEvent,
    admission_sequence: i64,
    sequence: i64,
) -> (RoutineInvocationSummary, RoutineInvocationEvent) {
    let event = RoutineInvocationEvent {
        sequence,
        invocation_id: admission.invocation_id.clone(),
        kind: RoutineEventKind::InvocationAdmitted,
        task_name: None,
        occurred_at_ms: admission.admitted_at_ms,
        error: None,
        values: None,
        session_references: None,
        admission: Some(admission_event),
    };
    let summary = RoutineInvocationSummary {
        admission_sequence,
        invocation_id: admission.invocation_id,
        execution_id: admission.execution_id,
        routine_name: admission.routine_name,
        routine_revision: admission.routine_revision,
        trigger_name: admission.trigger_name,
        trigger_kind: admission.trigger_kind,
        correlation_id: admission.correlation_id,
        metadata: admission.metadata,
        source_cursor: admission.source_cursor,
        declared_tasks: admission.declared_tasks,
        status: RoutineInvocationStatus::Admitted,
        admitted_at_ms: admission.admitted_at_ms,
        started_at_ms: None,
        completed_at_ms: None,
        updated_at_ms: admission.admitted_at_ms,
        terminal_error: None,
        latest_sequence: sequence,
    };
    (summary, event)
}

fn parse_summary<R>(row: &R) -> Result<RoutineInvocationSummary, RoutineStoreError>
where
    R: sqlx::Row,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
    String: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    Option<String>: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    i64: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    Option<i64>: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
{
    let tasks_json: String = row.try_get("declared_tasks_json").map_err(store_error)?;
    let tasks: VersionedTasks = decode_json(&tasks_json)?;
    if tasks.version != JSON_VERSION || tasks.tasks.len() > MAX_TASKS {
        return Err(invalid_stored_data());
    }
    let error_code: Option<String> = row.try_get("terminal_error_code").map_err(store_error)?;
    let error_message: Option<String> =
        row.try_get("terminal_error_message").map_err(store_error)?;
    let terminal_error = match (error_code, error_message) {
        (None, None) => None,
        (Some(code), Some(message)) => Some(RoutineFailure { code, message }),
        _ => return Err(invalid_stored_data()),
    };
    Ok(RoutineInvocationSummary {
        admission_sequence: row.try_get("admission_sequence").map_err(store_error)?,
        invocation_id: row.try_get("invocation_id").map_err(store_error)?,
        execution_id: row.try_get("execution_id").map_err(store_error)?,
        routine_name: row.try_get("routine_name").map_err(store_error)?,
        routine_revision: row.try_get("routine_revision").map_err(store_error)?,
        trigger_name: row.try_get("trigger_name").map_err(store_error)?,
        trigger_kind: RoutineTriggerKind::parse(
            &row.try_get::<String, _>("trigger_kind")
                .map_err(store_error)?,
        )?,
        correlation_id: row.try_get("correlation_id").map_err(store_error)?,
        metadata: serde_json::from_str(
            &row.try_get::<String, _>("metadata_json")
                .map_err(store_error)?,
        )
        .map_err(store_error)?,
        source_cursor: row.try_get("source_cursor").map_err(store_error)?,
        declared_tasks: tasks.tasks,
        status: RoutineInvocationStatus::parse(
            &row.try_get::<String, _>("status").map_err(store_error)?,
        )?,
        admitted_at_ms: row.try_get("admitted_at_ms").map_err(store_error)?,
        started_at_ms: row.try_get("started_at_ms").map_err(store_error)?,
        completed_at_ms: row.try_get("completed_at_ms").map_err(store_error)?,
        updated_at_ms: row.try_get("updated_at_ms").map_err(store_error)?,
        terminal_error,
        latest_sequence: row
            .try_get::<Option<i64>, _>("latest_sequence")
            .map_err(store_error)?
            .ok_or_else(invalid_stored_data)?,
    })
}

fn parse_event<R>(row: &R) -> Result<RoutineInvocationEvent, RoutineStoreError>
where
    R: sqlx::Row,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
    String: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    Option<String>: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    i64: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
{
    let error_code: Option<String> = row.try_get("error_code").map_err(store_error)?;
    let error_message: Option<String> = row.try_get("error_message").map_err(store_error)?;
    let error = match (error_code, error_message) {
        (None, None) => None,
        (Some(code), Some(message)) => Some(RoutineFailure { code, message }),
        _ => return Err(invalid_stored_data()),
    };
    let values_json: Option<String> = row.try_get("values_json").map_err(store_error)?;
    let sessions_json: Option<String> = row
        .try_get("session_references_json")
        .map_err(store_error)?;
    let kind = RoutineEventKind::parse(
        &row.try_get::<String, _>("event_kind")
            .map_err(store_error)?,
    )?;
    let (values, admission) = if kind == RoutineEventKind::InvocationAdmitted {
        let admission = values_json
            .as_deref()
            .map(decode_admission)
            .transpose()?
            .ok_or_else(invalid_stored_data)?;
        (None, Some(admission))
    } else {
        (values_json.as_deref().map(decode_values).transpose()?, None)
    };
    Ok(RoutineInvocationEvent {
        sequence: row.try_get("sequence").map_err(store_error)?,
        invocation_id: row.try_get("invocation_id").map_err(store_error)?,
        kind,
        task_name: row.try_get("task_name").map_err(store_error)?,
        occurred_at_ms: row.try_get("occurred_at_ms").map_err(store_error)?,
        error,
        values,
        session_references: sessions_json.as_deref().map(decode_sessions).transpose()?,
        admission,
    })
}

fn committed_event(
    sequence: i64,
    invocation_id: &str,
    event: NewEvent<'_>,
) -> Result<RoutineInvocationEvent, RoutineStoreError> {
    Ok(RoutineInvocationEvent {
        sequence,
        invocation_id: invocation_id.to_owned(),
        kind: event.kind,
        task_name: event.task_name.map(str::to_owned),
        occurred_at_ms: event.occurred_at_ms,
        error: event.error.cloned(),
        values: event
            .values_json
            .as_deref()
            .map(decode_values)
            .transpose()?,
        session_references: event
            .sessions_json
            .as_deref()
            .map(decode_sessions)
            .transpose()?,
        admission: None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskState {
    Started,
    Succeeded,
    Skipped,
    Blocked,
    Failed,
    Interrupted,
}

fn task_state(events: &[RoutineInvocationEvent], task_name: &str) -> Option<TaskState> {
    events
        .iter()
        .filter(|event| event.task_name.as_deref() == Some(task_name))
        .filter_map(|event| match event.kind {
            RoutineEventKind::TaskStarted => Some(TaskState::Started),
            RoutineEventKind::TaskSucceeded => Some(TaskState::Succeeded),
            RoutineEventKind::TaskSkipped => Some(TaskState::Skipped),
            RoutineEventKind::TaskBlocked => Some(TaskState::Blocked),
            RoutineEventKind::TaskFailed => Some(TaskState::Failed),
            RoutineEventKind::TaskInterrupted => Some(TaskState::Interrupted),
            _ => None,
        })
        .next_back()
}

fn validate_admission(admission: &RoutineAdmission) -> Result<(), RoutineStoreError> {
    validate_metadata(&admission.metadata)?;
    for (value, maximum, message) in [
        (
            admission.invocation_id.as_str(),
            512,
            "routine invocation id is invalid",
        ),
        (
            admission.execution_id.as_str(),
            512,
            "routine execution id is invalid",
        ),
        (
            admission.routine_name.as_str(),
            256,
            "routine name is invalid",
        ),
        (
            admission.routine_revision.as_str(),
            256,
            "routine revision is invalid",
        ),
        (
            admission.trigger_name.as_str(),
            256,
            "routine trigger name is invalid",
        ),
        (
            admission.correlation_id.as_str(),
            512,
            "routine correlation id is invalid",
        ),
    ] {
        if value.is_empty() || value.chars().count() > maximum {
            return Err(RoutineStoreError::Invalid(message));
        }
    }
    if admission.declared_tasks.len() > MAX_TASKS {
        return Err(RoutineStoreError::Invalid(
            "routine has too many declared tasks",
        ));
    }
    let mut names = std::collections::BTreeSet::new();
    for task in &admission.declared_tasks {
        if task.name.is_empty()
            || task.name.chars().count() > 256
            || task.task_type.is_empty()
            || !names.insert(&task.name)
        {
            return Err(RoutineStoreError::Invalid(
                "routine task descriptor is invalid",
            ));
        }
    }
    if admission
        .source_cursor
        .as_ref()
        .is_some_and(|cursor| cursor.is_empty() || cursor.len() > 1_024)
    {
        return Err(RoutineStoreError::Invalid(
            "routine source cursor is invalid",
        ));
    }
    Ok(())
}

pub(crate) fn validate_metadata(
    metadata: &BTreeMap<String, String>,
) -> Result<(), RoutineStoreError> {
    if metadata.len() > MAX_METADATA_ENTRIES
        || metadata.iter().any(|(key, value)| {
            key.is_empty()
                || key.len() > MAX_METADATA_KEY_BYTES
                || value.len() > MAX_METADATA_VALUE_BYTES
        })
    {
        return Err(RoutineStoreError::Invalid("routine metadata is invalid"));
    }
    Ok(())
}

fn push_metadata_filters<'args, DB: sqlx::Database>(
    query: &mut QueryBuilder<'args, DB>,
    filters: &'args BTreeMap<String, String>,
    invocation_column: &str,
    metadata_table: &str,
) where
    String: sqlx::Encode<'args, DB> + sqlx::Type<DB>,
{
    for (key, value) in filters {
        query
            .push(" AND EXISTS (SELECT 1 FROM ")
            .push(metadata_table)
            .push(" AS mf WHERE mf.invocation_id = ")
            .push(invocation_column)
            .push(" AND mf.metadata_key = ")
            .push_bind(key.clone())
            .push(" AND mf.metadata_value = ")
            .push_bind(value.clone())
            .push(")");
    }
}

fn validate_failure(failure: &RoutineFailure) -> Result<(), RoutineStoreError> {
    if failure.code.is_empty()
        || failure.code.len() > MAX_ERROR_CODE_BYTES
        || failure.message.len() > MAX_ERROR_MESSAGE_BYTES
    {
        return Err(RoutineStoreError::Invalid("routine failure is invalid"));
    }
    Ok(())
}

fn encode_values(values: &[StoredRoutineResult]) -> Result<Option<String>, RoutineStoreError> {
    if values.is_empty() {
        Ok(None)
    } else {
        validate_results(
            values,
            RoutineStoreError::Invalid("routine result is invalid"),
        )?;
        encode_json(&VersionedValues {
            version: JSON_VERSION,
            values: values.to_vec(),
        })
        .map(Some)
    }
}

fn encode_sessions(
    sessions: &BTreeMap<String, String>,
) -> Result<Option<String>, RoutineStoreError> {
    if sessions.is_empty() {
        return Ok(None);
    }
    if sessions
        .iter()
        .any(|(name, id)| name.is_empty() || id.is_empty())
    {
        return Err(RoutineStoreError::Invalid(
            "routine session reference is invalid",
        ));
    }
    encode_json(&VersionedSessionReferences {
        version: JSON_VERSION,
        sessions: sessions.clone(),
    })
    .map(Some)
}

fn decode_values(json: &str) -> Result<Vec<StoredRoutineResult>, RoutineStoreError> {
    let payload: VersionedValues = decode_json(json)?;
    if payload.version != JSON_VERSION {
        return Err(invalid_stored_data());
    }
    validate_results(&payload.values, invalid_stored_data())?;
    Ok(payload.values)
}

fn decode_admission(json: &str) -> Result<RoutineAdmissionEvent, RoutineStoreError> {
    let payload: VersionedAdmission = decode_json(json)?;
    if payload.version != JSON_VERSION
        || payload.admission.admission_sequence <= 0
        || payload.admission.routine_name.is_empty()
        || payload.admission.routine_revision.is_empty()
        || payload.admission.trigger_name.is_empty()
        || payload.admission.correlation_id.is_empty()
        || payload.admission.declared_tasks.len() > MAX_TASKS
        || validate_metadata(&payload.admission.metadata).is_err()
    {
        return Err(invalid_stored_data());
    }
    let mut names = std::collections::BTreeSet::new();
    if payload
        .admission
        .declared_tasks
        .iter()
        .any(|task| task.name.is_empty() || task.task_type.is_empty() || !names.insert(&task.name))
    {
        return Err(invalid_stored_data());
    }
    Ok(payload.admission)
}

fn validate_results(
    values: &[StoredRoutineResult],
    error: RoutineStoreError,
) -> Result<(), RoutineStoreError> {
    let mut names = std::collections::BTreeSet::new();
    if values.iter().any(|result| {
        result.name.is_empty()
            || result.name.chars().count() > MAX_RESULT_NAME_CHARS
            || !names.insert(&result.name)
    }) {
        return Err(error);
    }
    Ok(())
}

fn decode_sessions(json: &str) -> Result<BTreeMap<String, String>, RoutineStoreError> {
    let payload: VersionedSessionReferences = decode_json(json)?;
    if payload.version != JSON_VERSION {
        return Err(invalid_stored_data());
    }
    Ok(payload.sessions)
}

fn encode_json<T: Serialize>(value: &T) -> Result<String, RoutineStoreError> {
    let encoded = serde_json::to_string(value)
        .map_err(|_| RoutineStoreError::Invalid("routine JSON payload is invalid"))?;
    if encoded.len() > MAX_JSON_BYTES {
        return Err(RoutineStoreError::Invalid(
            "routine JSON payload is too large",
        ));
    }
    Ok(encoded)
}

fn decode_json<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T, RoutineStoreError> {
    if json.len() > MAX_JSON_BYTES {
        return Err(invalid_stored_data());
    }
    serde_json::from_str(json).map_err(|_| invalid_stored_data())
}

fn invalid_stored_data() -> RoutineStoreError {
    RoutineStoreError::Persistence
}
fn store_error<E>(_error: E) -> RoutineStoreError {
    RoutineStoreError::Persistence
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{DatabaseConfig, Persistence};

    async fn store() -> (Persistence, RoutineInvocationStore) {
        let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
            .await
            .unwrap();
        let store = persistence.routine_invocation_store();
        (persistence, store)
    }

    fn admission(id: &str, task_names: &[&str]) -> RoutineAdmission {
        RoutineAdmission {
            invocation_id: id.to_owned(),
            execution_id: "execution-shared".to_owned(),
            routine_name: "compact".to_owned(),
            routine_revision: "sha256:revision".to_owned(),
            trigger_name: "manual".to_owned(),
            trigger_kind: RoutineTriggerKind::Manual,
            correlation_id: "correlation-shared".to_owned(),
            metadata: BTreeMap::from([(
                SOURCE_SESSION_METADATA_KEY.to_owned(),
                "source".to_owned(),
            )]),
            source_cursor: Some("cursor-1".to_owned()),
            declared_tasks: task_names
                .iter()
                .map(|name| RoutineTaskDescriptor {
                    name: (*name).to_owned(),
                    task_type: "session_template".to_owned(),
                })
                .collect(),
            admitted_at_ms: 10,
        }
    }

    fn failure() -> RoutineFailure {
        RoutineFailure {
            code: "provider_failed".to_owned(),
            message: "safe detail".to_owned(),
        }
    }

    #[tokio::test]
    async fn admission_and_task_lifecycle_round_trip_versioned_json() {
        let (persistence, store) = store().await;
        let (summary, admitted) = store.admit(admission("one", &["prepare"])).await.unwrap();
        assert_eq!(summary.latest_sequence, admitted.sequence);
        assert!(admitted.sequence > 0);
        let admission_payload = admitted.admission.as_ref().unwrap();
        assert_eq!(
            admission_payload.admission_sequence,
            summary.admission_sequence
        );
        assert_eq!(admission_payload.routine_name, "compact");
        assert_eq!(admission_payload.routine_revision, "sha256:revision");
        assert_eq!(admission_payload.trigger_kind, RoutineTriggerKind::Manual);
        assert_eq!(
            admission_payload.metadata.get(SOURCE_SESSION_METADATA_KEY),
            Some(&"source".to_owned())
        );
        assert_eq!(
            admission_payload
                .declared_tasks
                .iter()
                .map(|task| task.name.as_str())
                .collect::<Vec<_>>(),
            ["prepare"]
        );
        let restarted_store = persistence.routine_invocation_store();
        let replayed = restarted_store
            .events_after(0, 1, &BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(replayed[0].admission, admitted.admission);
        assert_eq!(replayed[0].sequence, admitted.sequence);
        assert!(replayed[0].sequence > 0);
        store.start_invocation("one", 20).await.unwrap();
        let sessions = BTreeMap::from([("helper".to_owned(), "session-1".to_owned())]);
        store
            .transition_task(
                "one",
                "prepare",
                RoutineTaskTransition::Started {
                    session_references: sessions.clone(),
                },
                30,
            )
            .await
            .unwrap();
        let task_values = vec![
            StoredRoutineResult {
                name: "zeta".to_owned(),
                value: StoredRoutineValue::Boolean(true),
            },
            StoredRoutineResult {
                name: "alpha".to_owned(),
                value: StoredRoutineValue::String("exact".to_owned()),
            },
        ];
        let succeeded = store
            .transition_task(
                "one",
                "prepare",
                RoutineTaskTransition::Succeeded {
                    values: task_values.clone(),
                    session_references: sessions.clone(),
                },
                40,
            )
            .await
            .unwrap();
        assert_eq!(succeeded.values, Some(task_values));
        assert_eq!(succeeded.session_references, Some(sessions));
        let invocation_values = vec![
            StoredRoutineResult {
                name: "second".to_owned(),
                value: StoredRoutineValue::String("two".to_owned()),
            },
            StoredRoutineResult {
                name: "first".to_owned(),
                value: StoredRoutineValue::Boolean(false),
            },
        ];
        let finished = store
            .finish_success("one", invocation_values.clone(), 50)
            .await
            .unwrap();
        assert_eq!(finished.values, Some(invocation_values));
        let stored = store.get("one").await.unwrap().unwrap();
        assert_eq!(stored.status, RoutineInvocationStatus::Succeeded);
        assert_eq!(stored.declared_tasks, summary.declared_tasks);
        assert_eq!(store.events_for_invocation("one").await.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn admission_event_persists_the_independent_aggregate_sequence() {
        let (_persistence, store) = store().await;
        let (first, _) = store
            .admit(admission("first-admission", &[]))
            .await
            .unwrap();
        store.start_invocation("first-admission", 20).await.unwrap();
        store
            .finish_failure("first-admission", failure(), 30)
            .await
            .unwrap();

        let (second, second_event) = store
            .admit(admission("second-admission", &[]))
            .await
            .unwrap();
        assert_eq!(second.admission_sequence, first.admission_sequence + 1);
        assert_ne!(second.admission_sequence, second_event.sequence);
        assert_eq!(
            second_event.admission.as_ref().unwrap().admission_sequence,
            second.admission_sequence
        );
        let reconstructed = store
            .events_for_invocation("second-admission")
            .await
            .unwrap();
        assert_eq!(reconstructed, [second_event]);
        assert_eq!(
            reconstructed[0]
                .admission
                .as_ref()
                .unwrap()
                .admission_sequence,
            second.admission_sequence
        );
    }

    #[tokio::test]
    async fn detail_reads_summary_and_complete_journal_from_one_snapshot() {
        let (persistence, store) = store().await;
        assert_eq!(
            store.detail("missing").await.unwrap_err(),
            RoutineStoreError::NotFound
        );
        store.admit(admission("detail", &[])).await.unwrap();
        store.start_invocation("detail", 20).await.unwrap();
        let detail = store.detail("detail").await.unwrap();
        assert_eq!(detail.events.len(), 2);
        assert_eq!(
            detail.events.last().unwrap().sequence,
            detail.summary.latest_sequence
        );
        assert_eq!(
            detail.events[0]
                .admission
                .as_ref()
                .unwrap()
                .admission_sequence,
            detail.summary.admission_sequence
        );

        sqlx::query("UPDATE routine_invocations SET latest_sequence = latest_sequence + 100 WHERE invocation_id = 'detail'")
            .execute(persistence.sqlite_pool().unwrap())
            .await
            .unwrap();
        assert_eq!(
            store.detail("detail").await.unwrap_err(),
            RoutineStoreError::Persistence
        );
    }

    #[tokio::test]
    async fn terminal_cas_rejects_regression_without_partial_event() {
        let (_persistence, store) = store().await;
        store.admit(admission("terminal", &[])).await.unwrap();
        store.start_invocation("terminal", 20).await.unwrap();
        store
            .finish_success("terminal", Vec::new(), 30)
            .await
            .unwrap();
        let before = store.events_for_invocation("terminal").await.unwrap();
        assert_eq!(
            store
                .finish_failure("terminal", failure(), 40)
                .await
                .unwrap_err(),
            RoutineStoreError::Conflict
        );
        assert_eq!(
            store.events_for_invocation("terminal").await.unwrap(),
            before
        );
        assert_eq!(
            store.get("terminal").await.unwrap().unwrap().status,
            RoutineInvocationStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn task_failure_and_invocation_failure_commit_together() {
        let (persistence, store) = store().await;
        store.admit(admission("failure", &["task"])).await.unwrap();
        store.start_invocation("failure", 20).await.unwrap();
        store
            .transition_task(
                "failure",
                "task",
                RoutineTaskTransition::Started {
                    session_references: BTreeMap::new(),
                },
                30,
            )
            .await
            .unwrap();
        let committed = store
            .fail_task_and_invocation("failure", "task", failure(), 40)
            .await
            .unwrap();
        assert!(committed[1].sequence > committed[0].sequence);
        assert_eq!(
            store.get("failure").await.unwrap().unwrap().latest_sequence,
            committed[1].sequence
        );

        sqlx::query("CREATE TRIGGER reject_invocation_failed BEFORE INSERT ON routine_invocation_events WHEN NEW.event_kind = 'invocation_failed' BEGIN SELECT RAISE(ABORT, 'reject'); END")
            .execute(persistence.sqlite_pool().unwrap()).await.unwrap();
        store.admit(admission("rollback", &["task"])).await.unwrap();
        store.start_invocation("rollback", 50).await.unwrap();
        store
            .transition_task(
                "rollback",
                "task",
                RoutineTaskTransition::Started {
                    session_references: BTreeMap::new(),
                },
                60,
            )
            .await
            .unwrap();
        let before = store.events_for_invocation("rollback").await.unwrap();
        assert_eq!(
            store
                .fail_task_and_invocation("rollback", "task", failure(), 70)
                .await
                .unwrap_err(),
            RoutineStoreError::Persistence
        );
        assert_eq!(
            store.events_for_invocation("rollback").await.unwrap(),
            before
        );
        assert_eq!(
            store.get("rollback").await.unwrap().unwrap().status,
            RoutineInvocationStatus::Running
        );
    }

    #[tokio::test]
    async fn admission_and_stale_transition_roll_back_without_partial_state() {
        let (persistence, store) = store().await;
        let pool = persistence.sqlite_pool().unwrap();
        sqlx::query("CREATE TRIGGER reject_admission_event BEFORE INSERT ON routine_invocation_events WHEN NEW.event_kind = 'invocation_admitted' BEGIN SELECT RAISE(ABORT, 'reject'); END")
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            store.admit(admission("rejected", &[])).await.unwrap_err(),
            RoutineStoreError::Persistence
        );
        assert!(store.get("rejected").await.unwrap().is_none());
        sqlx::query("DROP TRIGGER reject_admission_event")
            .execute(pool)
            .await
            .unwrap();

        let (stale, _) = store.admit(admission("stale", &[])).await.unwrap();
        store.start_invocation("stale", 20).await.unwrap();
        let before = store.events_for_invocation("stale").await.unwrap();
        let stale_failure = failure();
        assert_eq!(
            store
                .append(
                    &stale,
                    NewEvent {
                        kind: RoutineEventKind::InvocationInterrupted,
                        task_name: None,
                        occurred_at_ms: 30,
                        error: Some(&stale_failure),
                        values_json: None,
                        sessions_json: None,
                    },
                    RoutineInvocationStatus::Interrupted,
                    true,
                )
                .await
                .unwrap_err(),
            RoutineStoreError::Conflict
        );
        assert_eq!(store.events_for_invocation("stale").await.unwrap(), before);
        assert_eq!(
            store.get("stale").await.unwrap().unwrap().status,
            RoutineInvocationStatus::Running
        );
    }

    #[tokio::test]
    async fn replay_accepts_nonconsecutive_sequences_and_reports_bounds() {
        let (persistence, store) = store().await;
        store.admit(admission("gap", &[])).await.unwrap();
        let pool = persistence.sqlite_pool().unwrap();
        sqlx::query("UPDATE sqlite_sequence SET seq = 40 WHERE name = 'routine_invocation_events'")
            .execute(pool)
            .await
            .unwrap();
        let started = store.start_invocation("gap", 20).await.unwrap();
        assert_eq!(started.sequence, 41);
        let replay = store.events_after(1, 20, &BTreeMap::new()).await.unwrap();
        assert_eq!(
            replay
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            [41]
        );
        assert_eq!(
            store.sequence_bounds().await.unwrap(),
            RoutineSequenceBounds {
                low: Some(1),
                high: Some(41)
            }
        );
        assert_eq!(store.high_water().await.unwrap(), Some(41));
    }

    #[tokio::test]
    async fn admission_keyset_order_is_stable_after_terminal_updates() {
        let (_persistence, store) = store().await;
        let (first, _) = store.admit(admission("first", &[])).await.unwrap();
        let (second, _) = store.admit(admission("second", &[])).await.unwrap();
        let page = store.list(None, 1, &BTreeMap::new()).await.unwrap();
        assert_eq!(page[0].invocation_id, "second");
        store
            .finish_interruption("first", failure(), 100)
            .await
            .unwrap();
        let next = store
            .list(Some(second.admission_sequence), 10, &BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(
            next.iter()
                .map(|item| item.invocation_id.as_str())
                .collect::<Vec<_>>(),
            ["first"]
        );
        assert_eq!(next[0].admission_sequence, first.admission_sequence);
    }

    #[tokio::test]
    async fn metadata_round_trips_and_exact_and_filters_apply_before_limits() {
        let (_persistence, store) = store().await;
        let mut first = admission("metadata-first", &[]);
        first.metadata = BTreeMap::from([
            ("environment".to_owned(), "production".to_owned()),
            (
                SOURCE_SESSION_METADATA_KEY.to_owned(),
                "source-a".to_owned(),
            ),
        ]);
        let (first_summary, first_admitted) = store.admit(first).await.unwrap();

        let mut second = admission("metadata-second", &[]);
        second.metadata = BTreeMap::from([
            ("environment".to_owned(), "development".to_owned()),
            (
                SOURCE_SESSION_METADATA_KEY.to_owned(),
                "source-a".to_owned(),
            ),
        ]);
        store.admit(second).await.unwrap();

        let mut third = admission("metadata-third", &[]);
        third.metadata = BTreeMap::from([
            ("environment".to_owned(), "production".to_owned()),
            (
                SOURCE_SESSION_METADATA_KEY.to_owned(),
                "source-b".to_owned(),
            ),
        ]);
        store.admit(third).await.unwrap();

        let filters = BTreeMap::from([
            ("environment".to_owned(), "production".to_owned()),
            (
                SOURCE_SESSION_METADATA_KEY.to_owned(),
                "source-a".to_owned(),
            ),
        ]);
        let page = store.list(None, 1, &filters).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].invocation_id, "metadata-first");
        assert_eq!(page[0].metadata, first_summary.metadata);
        assert_eq!(
            first_admitted.admission.as_ref().unwrap().metadata,
            first_summary.metadata
        );
        assert_eq!(
            store.get("metadata-first").await.unwrap().unwrap().metadata,
            first_summary.metadata
        );

        let started = store.start_invocation("metadata-first", 20).await.unwrap();
        assert!(started.sequence > first_admitted.sequence + 1);
        let replay = store
            .events_after(first_admitted.sequence, 1, &filters)
            .await
            .unwrap();
        assert_eq!(replay, [started]);
    }

    #[tokio::test]
    async fn reconcile_discovers_and_interrupts_only_nonterminal_records() {
        let (_persistence, store) = store().await;
        store
            .admit(admission("admitted", &["pending"]))
            .await
            .unwrap();
        store
            .admit(admission("running", &["pending", "started"]))
            .await
            .unwrap();
        store.start_invocation("running", 20).await.unwrap();
        store
            .transition_task(
                "running",
                "started",
                RoutineTaskTransition::Started {
                    session_references: BTreeMap::new(),
                },
                25,
            )
            .await
            .unwrap();
        store.admit(admission("done", &[])).await.unwrap();
        store.finish_failure("done", failure(), 30).await.unwrap();
        assert_eq!(store.nonterminal(10).await.unwrap().len(), 2);
        let reconciled = store.reconcile_unfinished(failure(), 40).await.unwrap();
        assert_eq!(reconciled.len(), 5);
        assert_eq!(
            reconciled
                .iter()
                .filter(|event| event.kind == RoutineEventKind::TaskInterrupted)
                .count(),
            3
        );
        assert_eq!(
            reconciled
                .iter()
                .filter(|event| event.kind == RoutineEventKind::InvocationInterrupted)
                .count(),
            2
        );
        assert!(store.nonterminal(10).await.unwrap().is_empty());
        assert!(
            store
                .reconcile_unfinished(failure(), 50)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reconcile_repeats_bounded_batches_until_every_record_is_terminal() {
        let (_persistence, store) = store().await;
        let count = RECONCILE_BATCH_SIZE + 3;
        for index in 0..count {
            store
                .admit(admission(&format!("batch-{index}"), &["pending"]))
                .await
                .unwrap();
        }

        let committed = store.reconcile_unfinished(failure(), 100).await.unwrap();
        assert_eq!(committed.len(), count * 2);
        for events in committed.chunks_exact(2) {
            assert_eq!(events[0].kind, RoutineEventKind::TaskInterrupted);
            assert_eq!(events[0].task_name.as_deref(), Some("pending"));
            assert_eq!(events[1].kind, RoutineEventKind::InvocationInterrupted);
            assert!(events[0].sequence < events[1].sequence);
            assert_eq!(events[0].invocation_id, events[1].invocation_id);
        }
        assert!(store.nonterminal(count + 1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn interruption_terminalizes_pending_and_started_tasks_in_one_ordered_batch() {
        let (_persistence, store) = store().await;
        store
            .admit(admission(
                "interrupt-batch",
                &["pending", "started", "skipped", "succeeded"],
            ))
            .await
            .unwrap();
        store.start_invocation("interrupt-batch", 20).await.unwrap();
        store
            .transition_task(
                "interrupt-batch",
                "started",
                RoutineTaskTransition::Started {
                    session_references: BTreeMap::new(),
                },
                30,
            )
            .await
            .unwrap();
        store
            .transition_task(
                "interrupt-batch",
                "skipped",
                RoutineTaskTransition::Skipped,
                40,
            )
            .await
            .unwrap();
        store
            .transition_task(
                "interrupt-batch",
                "succeeded",
                RoutineTaskTransition::Started {
                    session_references: BTreeMap::new(),
                },
                50,
            )
            .await
            .unwrap();
        store
            .transition_task(
                "interrupt-batch",
                "succeeded",
                RoutineTaskTransition::Succeeded {
                    values: Vec::new(),
                    session_references: BTreeMap::new(),
                },
                60,
            )
            .await
            .unwrap();

        let committed = store
            .finish_interruption("interrupt-batch", failure(), 70)
            .await
            .unwrap();
        assert_eq!(
            committed
                .iter()
                .map(|event| (event.kind, event.task_name.as_deref()))
                .collect::<Vec<_>>(),
            [
                (RoutineEventKind::TaskInterrupted, Some("pending")),
                (RoutineEventKind::TaskInterrupted, Some("started")),
                (RoutineEventKind::InvocationInterrupted, None),
            ]
        );
        assert!(
            committed
                .windows(2)
                .all(|events| events[0].sequence < events[1].sequence)
        );
        let stored = store
            .events_for_invocation("interrupt-batch")
            .await
            .unwrap();
        assert_eq!(
            stored
                .iter()
                .filter(|event| event.task_name.as_deref() == Some("skipped"))
                .count(),
            1
        );
        assert_eq!(
            stored
                .iter()
                .filter(|event| event.task_name.as_deref() == Some("succeeded"))
                .count(),
            2
        );
        let summary = store.get("interrupt-batch").await.unwrap().unwrap();
        assert_eq!(summary.status, RoutineInvocationStatus::Interrupted);
        assert_eq!(summary.latest_sequence, committed.last().unwrap().sequence);
    }

    #[tokio::test]
    async fn interruption_rolls_back_all_task_events_on_journal_failure() {
        let (persistence, store) = store().await;
        store
            .admit(admission("interrupt-rollback", &["first", "second"]))
            .await
            .unwrap();
        store
            .start_invocation("interrupt-rollback", 20)
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER reject_second_interruption BEFORE INSERT ON routine_invocation_events WHEN NEW.event_kind = 'task_interrupted' AND NEW.task_name = 'second' BEGIN SELECT RAISE(ABORT, 'reject'); END")
            .execute(persistence.sqlite_pool().unwrap())
            .await
            .unwrap();
        let before = store
            .events_for_invocation("interrupt-rollback")
            .await
            .unwrap();
        assert_eq!(
            store
                .finish_interruption("interrupt-rollback", failure(), 30)
                .await
                .unwrap_err(),
            RoutineStoreError::Persistence
        );
        assert_eq!(
            store
                .events_for_invocation("interrupt-rollback")
                .await
                .unwrap(),
            before
        );
        assert_eq!(
            store
                .get("interrupt-rollback")
                .await
                .unwrap()
                .unwrap()
                .status,
            RoutineInvocationStatus::Running
        );
    }

    #[tokio::test]
    async fn closed_task_transitions_are_reconstructed_and_cannot_regress() {
        let (_persistence, store) = store().await;
        store
            .admit(admission("task-states", &["skip", "block", "interrupt"]))
            .await
            .unwrap();
        store.start_invocation("task-states", 20).await.unwrap();
        store
            .transition_task("task-states", "skip", RoutineTaskTransition::Skipped, 30)
            .await
            .unwrap();
        store
            .transition_task(
                "task-states",
                "block",
                RoutineTaskTransition::Blocked(failure()),
                40,
            )
            .await
            .unwrap();
        store
            .transition_task(
                "task-states",
                "interrupt",
                RoutineTaskTransition::Started {
                    session_references: BTreeMap::new(),
                },
                50,
            )
            .await
            .unwrap();
        store
            .transition_task(
                "task-states",
                "interrupt",
                RoutineTaskTransition::Interrupted(failure()),
                60,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .transition_task(
                    "task-states",
                    "skip",
                    RoutineTaskTransition::Started {
                        session_references: BTreeMap::new(),
                    },
                    70,
                )
                .await
                .unwrap_err(),
            RoutineStoreError::Conflict
        );
    }

    #[tokio::test]
    async fn rejects_oversized_and_unversioned_boundary_data() {
        let (persistence, store) = store().await;
        let mut oversized = admission("oversized", &[]);
        oversized.source_cursor = Some("x".repeat(1_025));
        assert!(matches!(
            store.admit(oversized).await,
            Err(RoutineStoreError::Invalid(_))
        ));
        store.admit(admission("corrupt", &[])).await.unwrap();
        sqlx::query("UPDATE routine_invocations SET declared_tasks_json = '{\"version\":2,\"tasks\":[]}' WHERE invocation_id = 'corrupt'")
            .execute(persistence.sqlite_pool().unwrap()).await.unwrap();
        assert_eq!(
            store.get("corrupt").await.unwrap_err(),
            RoutineStoreError::Persistence
        );
    }

    #[tokio::test]
    async fn rejects_duplicate_result_names_at_write_and_read_boundaries() {
        let (persistence, store) = store().await;
        store
            .admit(admission("result-validation", &["task"]))
            .await
            .unwrap();
        store
            .start_invocation("result-validation", 20)
            .await
            .unwrap();
        store
            .transition_task(
                "result-validation",
                "task",
                RoutineTaskTransition::Started {
                    session_references: BTreeMap::new(),
                },
                30,
            )
            .await
            .unwrap();
        let duplicate = vec![
            StoredRoutineResult {
                name: "same".to_owned(),
                value: StoredRoutineValue::Boolean(true),
            },
            StoredRoutineResult {
                name: "same".to_owned(),
                value: StoredRoutineValue::Boolean(false),
            },
        ];
        assert!(matches!(
            store
                .transition_task(
                    "result-validation",
                    "task",
                    RoutineTaskTransition::Succeeded {
                        values: duplicate,
                        session_references: BTreeMap::new(),
                    },
                    40,
                )
                .await,
            Err(RoutineStoreError::Invalid(_))
        ));
        store
            .transition_task(
                "result-validation",
                "task",
                RoutineTaskTransition::Succeeded {
                    values: vec![StoredRoutineResult {
                        name: "valid".to_owned(),
                        value: StoredRoutineValue::Boolean(true),
                    }],
                    session_references: BTreeMap::new(),
                },
                50,
            )
            .await
            .unwrap();

        let pool = persistence.sqlite_pool().unwrap();
        sqlx::query("UPDATE routine_invocation_events SET values_json = ? WHERE invocation_id = 'result-validation' AND event_kind = 'task_succeeded'")
            .bind(r#"{"version":1,"values":[{"name":"same","value":{"type":"boolean","value":true}},{"name":"same","value":{"type":"boolean","value":false}}]}"#)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            store
                .events_for_invocation("result-validation")
                .await
                .unwrap_err(),
            RoutineStoreError::Persistence
        );
        sqlx::query("UPDATE routine_invocation_events SET values_json = ? WHERE invocation_id = 'result-validation' AND event_kind = 'task_succeeded'")
            .bind(r#"{"values":[{"name":"valid","value":{"type":"boolean","value":true}}]}"#)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            store
                .events_for_invocation("result-validation")
                .await
                .unwrap_err(),
            RoutineStoreError::Persistence
        );
    }
}
