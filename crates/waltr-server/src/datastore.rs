//! First-party datastore authorization and application service.

#[cfg(all(test, feature = "test"))]
mod tests;
mod transfer;
use transfer::Transfers;
pub(crate) use transfer::{TransferChunk, TransferHandle, TransferOpen};

use crate::{access_auth::AuthenticatedPrincipal, resource_catalog::AgentDefinitionState};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex as SyncMutex},
};
use tokio::sync::Mutex;
use waltr_datastore::{Actor, Definition, Error, Kind, Namespace, object, relational};

pub(crate) const ROOT: &str = "waltr://data-stores";
pub(crate) const RESOURCE_TEMPLATES: &[(&str, &str, &str)] = &[
    ("{store}", "data-store", "Logical datastore and its schema."),
    (
        "{store}/objects/{key}",
        "data-object",
        "Object descriptor. A nested key occupies one URI component.",
    ),
    (
        "{store}/objects/{key}/content",
        "data-object-content",
        "Bounded object content.",
    ),
    (
        "{store}/collections/{collection}",
        "data-collection",
        "Collection field schema.",
    ),
    (
        "{store}/collections/{collection}/records/{id}",
        "data-record",
        "Record fields and attribution.",
    ),
];

pub(crate) fn is_resource(uri: &str) -> bool {
    uri == ROOT
        || uri
            .strip_prefix(ROOT)
            .is_some_and(|tail| tail.starts_with('/'))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObjectKey {
    store: String,
    key: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObjectWrite {
    store: String,
    key: String,
    content_base64: Option<String>,
    transfer: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObjectList {
    store: String,
    #[serde(default)]
    prefix: String,
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordKey {
    store: String,
    collection: String,
    id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordCreate {
    store: String,
    collection: String,
    id: Option<String>,
    fields: Map<String, Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordUpdate {
    store: String,
    collection: String,
    id: String,
    fields: Map<String, Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordFind {
    store: String,
    collection: String,
    #[serde(default, rename = "where")]
    filters: Map<String, Value>,
    sort: Option<SortField>,
    #[serde(default)]
    filters_by: Vec<FilterField>,
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SortField {
    field: String,
    #[serde(default)]
    descending: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FilterField {
    field: String,
    op: FilterOp,
    value: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
enum FilterOp {
    Eq,
    Lt,
    Lte,
    Gt,
    Gte,
    IsNull,
    IsMissing,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Link {
    store: String,
    relation: String,
    from_id: String,
    to_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RelationList {
    store: String,
    relation: String,
    from_id: Option<String>,
    to_id: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

fn input<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, Error> {
    serde_json::from_value(value).map_err(|_| Error::Invalid)
}

impl RecordFind {
    fn query(&self) -> Result<relational::Query, Error> {
        use relational::FilterOp as Native;
        if self.filters.values().any(Value::is_null)
            || self.filters_by.iter().any(|f| {
                matches!(f.op, FilterOp::IsNull | FilterOp::IsMissing) != f.value.is_none()
            })
        {
            return Err(Error::Invalid);
        }
        let filters = self
            .filters
            .iter()
            .map(|(field, value)| relational::Filter {
                field: field.clone(),
                op: Native::Eq,
                value: Some(value.clone()),
            })
            .chain(self.filters_by.iter().map(|f| relational::Filter {
                field: f.field.clone(),
                op: match f.op {
                    FilterOp::Eq => Native::Eq,
                    FilterOp::Lt => Native::Lt,
                    FilterOp::Lte => Native::Lte,
                    FilterOp::Gt => Native::Gt,
                    FilterOp::Gte => Native::Gte,
                    FilterOp::IsNull => Native::IsNull,
                    FilterOp::IsMissing => Native::IsMissing,
                },
                value: f.value.clone(),
            }))
            .collect();
        Ok(relational::Query {
            filters,
            cursor: self.cursor.clone(),
            limit: self.limit.unwrap_or(20),
            sort: self.sort.as_ref().map(|s| relational::Sort {
                field: s.field.clone(),
                descending: s.descending,
            }),
        })
    }
}

impl RelationList {
    fn traversal(&self) -> Result<(&str, relational::Traversal), Error> {
        use relational::Direction;
        let (id, direction) = match (self.from_id.as_deref(), self.to_id.as_deref()) {
            (Some(id), None) => (id, Direction::Outgoing),
            (None, Some(id)) => (id, Direction::Incoming),
            _ => return Err(Error::Invalid),
        };
        Ok((
            id,
            relational::Traversal {
                direction,
                cursor: self.cursor.clone(),
                limit: self.limit.unwrap_or(20),
            },
        ))
    }
}

pub(crate) struct DataStores {
    pub(crate) catalog: AgentDefinitionState,
    pub(crate) relational: relational::Relational,
    pub(crate) objects: object::Objects,
    transfers: Transfers,
    admission: Arc<SyncMutex<BTreeMap<String, usize>>>,
}

struct TransactionPermit {
    admission: Arc<SyncMutex<BTreeMap<String, usize>>>,
    owner: String,
}

impl Drop for TransactionPermit {
    fn drop(&mut self) {
        if let Ok(mut active) = self.admission.lock() {
            if let Some(count) = active.get_mut(&self.owner) {
                *count -= 1;
                if *count == 0 {
                    active.remove(&self.owner);
                }
            }
        }
    }
}

#[derive(Default)]
struct TransactionState {
    native: Option<relational::Transaction>,
    permit: Option<TransactionPermit>,
    stores: BTreeMap<String, uuid::Uuid>,
    principal: Option<AuthenticatedPrincipal>,
    failed: bool,
    finished: bool,
}

pub(crate) struct DataStoreTransaction {
    catalog: AgentDefinitionState,
    database: relational::Relational,
    state: Mutex<TransactionState>,
}

struct Selected {
    definition: Definition,
    activation: uuid::Uuid,
    namespace: Namespace,
    actor: Option<Actor>,
}

impl DataStoreTransaction {
    pub(crate) async fn mark_failed(&self) {
        self.state.lock().await.failed = true;
    }

    pub(crate) async fn finish(&self, invocation: &str, commit: bool) -> Result<(), Error> {
        let _fence = self.catalog.fence_data_store_commit().await;
        let (native, _permit, valid, principal) = {
            let mut state = self.state.lock().await;
            if state.finished {
                return Err(Error::Conflict);
            }
            state.finished = true;
            let snapshot = self.catalog.snapshot();
            let valid = !state.failed
                && state.principal.as_ref().is_none_or(live)
                && state.stores.iter().all(|(name, activation)| {
                    snapshot
                        .data_store(name)
                        .is_some_and(|store| store.activation() == *activation)
                });
            (state.native.take(), state.permit.take(), valid, state.principal.take())
        };
        let valid = valid && (!commit || match principal {
            Some(principal) => principal.revalidate_datastore_access().await,
            None => true,
        });
        if let Some(native) = native {
            if commit && valid {
                return match native.commit(invocation).await {
                    Err(Error::Uncertain)
                        if self.database.has_receipt(invocation).await == Ok(true) =>
                    {
                        Ok(())
                    }
                    result => result,
                };
            }
            native.rollback().await?;
        }
        if commit && !valid {
            Err(Error::Conflict)
        } else {
            Ok(())
        }
    }
}

fn live(principal: &AuthenticatedPrincipal) -> bool {
    use futures_util::FutureExt as _;
    principal.access_ended().now_or_never().is_none()
}

fn encode(value: &str) -> String {
    crate::mcp_service::encode_component(value)
}

fn decode(value: &str) -> Result<String, Error> {
    crate::mcp_service::decode_component(value).map_err(|_| Error::Invalid)
}

fn record_uri(store: &str, collection: &str, id: &str) -> String {
    format!(
        "{ROOT}/{}/collections/{}/records/{}",
        encode(store),
        encode(collection),
        encode(id)
    )
}

fn object_uri(store: &str, key: &str) -> String {
    format!("{ROOT}/{}/objects/{}", encode(store), encode(key))
}

fn object_value(store: &str, info: object::ObjectInfo) -> Value {
    let uri = object_uri(store, &info.key);
    json!({"key":info.key,"size":info.size,"uri":uri,"content_uri":format!("{uri}/content"),
        "attribution":info.attribution,"last_known_waltr_write":info.last_known_waltr_write})
}

fn object_cursor_scope(selected: &Selected, prefix: &str) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut hash = Sha256::new();
    hash.update(selected.activation.as_bytes());
    hash.update(selected.namespace.as_str());
    hash.update([0]);
    hash.update(prefix);
    hash.finalize().into()
}

fn object_cursor_encode(scope: &[u8; 32], key: &str) -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut bytes = scope.to_vec();
    bytes.extend_from_slice(key.as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

fn object_cursor_decode(scope: &[u8; 32], cursor: &str) -> Result<String, Error> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    if cursor.len() > 8192 {
        return Err(Error::Invalid);
    }
    let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| Error::Invalid)?;
    let key = bytes.strip_prefix(scope.as_slice()).ok_or(Error::Invalid)?;
    String::from_utf8(key.to_vec()).map_err(|_| Error::Invalid)
}

fn record_value(store: &str, collection: &str, record: relational::Record) -> Value {
    json!({"id":record.id,"uri":record_uri(store,collection,&record.id),
        "fields":record.fields,"attribution":record.attribution})
}

impl DataStores {
    pub(crate) async fn install_catalog_validation(&self) -> Result<(), Error> {
        self.catalog.install_data_store_storage(self.relational.clone()).await
            .map_err(|_| Error::Unavailable)
    }

    pub(crate) async fn commit_receipt(&self, invocation: &str) -> Result<bool, Error> {
        self.relational.has_receipt(invocation).await
    }

    pub(crate) fn begin(&self) -> DataStoreTransaction {
        DataStoreTransaction {
            catalog: self.catalog.clone(),
            database: self.relational.clone(),
            state: Mutex::default(),
        }
    }

    pub(crate) fn handles(tool: &str, action: &str) -> bool {
        matches!(
            (tool, action),
            (
                "query",
                "object.list" | "object.download_chunk" | "record.find" | "relation.list"
            ) | (
                "create",
                "object.create"
                    | "object.upload"
                    | "object.download"
                    | "record.create"
                    | "relation.link"
            ) | (
                "execute",
                "object.replace" | "object.upload_chunk" | "record.update"
            ) | (
                "destroy",
                "object.delete" | "object.close_transfer" | "record.delete" | "relation.unlink"
            )
        )
    }

    fn select(&self, principal: &AuthenticatedPrincipal, name: &str) -> Result<Selected, Error> {
        if !live(principal) {
            return Err(Error::NotFound);
        }
        waltr_datastore::identifier(name)?;
        let snapshot = self.catalog.snapshot();
        let store = snapshot.data_store(name).ok_or(Error::NotFound)?;
        let actor = principal.datastore_actor();
        let definition = store.definition().clone();
        let namespace =
            Namespace::resolve(name, &definition, actor.as_ref()).map_err(|_| Error::NotFound)?;
        Ok(Selected {
            definition,
            activation: store.activation(),
            namespace,
            actor,
        })
    }

    async fn enter<'a>(
        &'a self,
        principal: &AuthenticatedPrincipal,
        name: &str,
        tx: &'a DataStoreTransaction,
    ) -> Result<(Selected, tokio::sync::MutexGuard<'a, TransactionState>), Error> {
        let mut state = tx.state.lock().await;
        if state.failed
            || state.finished
            || state
                .principal
                .as_ref()
                .is_some_and(|p| !p.same_access_lease(principal))
        {
            return Err(Error::Conflict);
        }
        let selected = self.select(principal, name)?;
        if selected.definition.kind != Kind::Relational {
            return Err(Error::Invalid);
        }
        if state
            .stores
            .get(name)
            .is_some_and(|activation| *activation != selected.activation)
        {
            state.failed = true;
            return Err(Error::Conflict);
        }
        state.stores.insert(name.to_owned(), selected.activation);
        if state.native.is_none() {
            let owner = selected
                .actor
                .as_ref()
                .map_or("anonymous", Actor::as_str)
                .to_owned();
            let permit = {
                let mut active = self.admission.lock().map_err(|_| Error::Unavailable)?;
                if active.values().sum::<usize>() >= 6
                    || active.get(&owner).copied().unwrap_or(0) >= 2
                {
                    return Err(Error::Limit);
                }
                *active.entry(owner.clone()).or_default() += 1;
                TransactionPermit {
                    admission: self.admission.clone(),
                    owner,
                }
            };
            state.native = Some(self.relational.begin().await?);
            state.permit = Some(permit);
            state.principal = Some(principal.clone());
        }
        Ok((selected, state))
    }

    pub(crate) async fn call(
        &self,
        principal: &AuthenticatedPrincipal,
        tool: &str,
        action: &str,
        value: Value,
        tx: Option<&DataStoreTransaction>,
    ) -> Result<Value, Error> {
        if !Self::handles(tool, action) {
            return Err(Error::Invalid);
        }
        let _fence = self.catalog.fence_data_store_commit().await;
        if !principal.revalidate_datastore_access().await {
            if let Some(tx) = tx {
                tx.mark_failed().await;
            }
            return Err(Error::NotFound);
        }
        let mutation = tool != "query";
        let result = if action.starts_with("object.") {
            if tx.is_some() {
                Err(Error::Invalid)
            } else {
                self.object_call(principal, action, value).await
            }
        } else {
            self.relational_call(principal, action, value, tx).await
        };
        if mutation
            && result.is_err()
            && let Some(tx) = tx
        {
            tx.mark_failed().await;
        }
        result
    }

    async fn object_call(
        &self,
        principal: &AuthenticatedPrincipal,
        action: &str,
        value: Value,
    ) -> Result<Value, Error> {
        let name = value
            .get("store")
            .and_then(Value::as_str)
            .ok_or(Error::Invalid)?
            .to_owned();
        let selected = self.select(principal, &name)?;
        if selected.definition.kind != Kind::Object {
            return Err(Error::Invalid);
        }
        let ns = &selected.namespace;
        match action {
            "object.upload"
            | "object.download"
            | "object.upload_chunk"
            | "object.download_chunk"
            | "object.close_transfer" => {
                self.transfer_call(principal, &selected, &name, action, value)
                    .await
            }
            "object.list" => {
                let request: ObjectList = input(value)?;
                if request.store != name {
                    return Err(Error::Invalid);
                }
                let prefix = request.prefix.strip_suffix('/').unwrap_or(&request.prefix);
                let scope = object_cursor_scope(&selected, prefix);
                let after = request
                    .cursor
                    .as_deref()
                    .map(|cursor| object_cursor_decode(&scope, cursor))
                    .transpose()?;
                let page = self
                    .objects
                    .list(
                        ns,
                        (!prefix.is_empty()).then_some(prefix),
                        after.as_deref(),
                        request.limit.unwrap_or(20),
                    )
                    .await?;
                Ok(
                    json!({"items":page.items.into_iter().map(|item|object_value(&name,item)).collect::<Vec<_>>(),
                    "next_cursor":page.next_after.map(|key| object_cursor_encode(&scope, &key))}),
                )
            }
            "object.create" | "object.replace" => {
                let request: ObjectWrite = input(value)?;
                if request.store != name {
                    return Err(Error::Invalid);
                }
                let actor = selected.actor.as_ref().ok_or(Error::NotFound)?;
                let body: Box<dyn tokio::io::AsyncRead + Unpin + Send> =
                    match (request.content_base64, request.transfer) {
                        (Some(content), None) => {
                            Box::new(std::io::Cursor::new(decode_inline(&content)?))
                        }
                        (None, Some(id)) => Box::new(
                            self.transfers
                                .take_upload(
                                    uuid::Uuid::parse_str(&id).map_err(|_| Error::Invalid)?,
                                    &selected,
                                    actor,
                                    &request.key,
                                )
                                .await?,
                        ),
                        _ => return Err(Error::Invalid),
                    };
                let info = if action == "object.create" {
                    self.objects.create(ns, &request.key, actor, body).await?
                } else {
                    self.objects.replace(ns, &request.key, actor, body).await?
                };
                Ok(object_value(&name, info))
            }
            "object.delete" => {
                let request: ObjectKey = input(value)?;
                if request.store != name {
                    return Err(Error::Invalid);
                }
                let actor = selected.actor.as_ref().ok_or(Error::NotFound)?;
                self.objects.delete(ns, &request.key, actor).await?;
                Ok(json!({"deleted":object_uri(&name,&request.key)}))
            }
            _ => Err(Error::Invalid),
        }
    }

    async fn relational_call(
        &self,
        principal: &AuthenticatedPrincipal,
        action: &str,
        value: Value,
        tx: Option<&DataStoreTransaction>,
    ) -> Result<Value, Error> {
        let name = value
            .get("store")
            .and_then(Value::as_str)
            .ok_or(Error::Invalid)?
            .to_owned();
        let selected;
        let mut state;
        let transaction;
        if let Some(tx) = tx {
            (selected, state) = self.enter(principal, &name, tx).await?;
            transaction = Some(state.native.as_mut().ok_or(Error::Unavailable)?);
        } else {
            selected = self.select(principal, &name)?;
            transaction = None;
        }
        if selected.definition.kind != Kind::Relational {
            return Err(Error::Invalid);
        }
        let ns = &selected.namespace;
        let definition = &selected.definition;
        macro_rules! apply {
            ($method:ident($($arg:expr),* $(,)?)) => {{
                match transaction {
                    Some(tx) => tx.$method(ns, definition, $($arg),*).await,
                    None => self.relational.$method(ns, definition, $($arg),*).await,
                }
            }};
        }
        match action {
            "record.create" => {
                let request: RecordCreate = input(value)?;
                let actor = selected.actor.as_ref().ok_or(Error::NotFound)?;
                let id = request
                    .id
                    .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
                let result = apply!(create(actor, &request.collection, &id, &request.fields))?;
                Ok(record_value(&request.store, &request.collection, result))
            }
            "record.update" => {
                let request: RecordUpdate = input(value)?;
                let actor = selected.actor.as_ref().ok_or(Error::NotFound)?;
                let result = apply!(update(
                    actor,
                    &request.collection,
                    &request.id,
                    &request.fields
                ))?;
                Ok(record_value(&request.store, &request.collection, result))
            }
            "record.delete" => {
                let request: RecordKey = input(value)?;
                let actor = selected.actor.as_ref().ok_or(Error::NotFound)?;
                apply!(delete(actor, &request.collection, &request.id))?;
                Ok(json!({"deleted":record_uri(&request.store,&request.collection,&request.id)}))
            }
            "relation.link" | "relation.unlink" => {
                let request: Link = input(value)?;
                if request.store != name {
                    return Err(Error::Invalid);
                }
                let actor = selected.actor.as_ref().ok_or(Error::NotFound)?;
                if action == "relation.link" {
                    Ok(json!(apply!(link(
                        actor,
                        &request.relation,
                        &request.from_id,
                        &request.to_id
                    ))?))
                } else {
                    apply!(unlink(
                        actor,
                        &request.relation,
                        &request.from_id,
                        &request.to_id
                    ))?;
                    Ok(json!({"removed":true}))
                }
            }
            "record.find" => {
                let request: RecordFind = input(value)?;
                let query = request.query()?;
                let page = apply!(find(&request.collection, &query))?;
                Ok(
                    json!({"items":page.items.into_iter().map(|record|record_value(&request.store,&request.collection,record)).collect::<Vec<_>>(),
                    "next_cursor":page.cursor}),
                )
            }
            "relation.list" => {
                let request: RelationList = input(value)?;
                if request.store != name {
                    return Err(Error::Invalid);
                }
                let (id, traversal) = request.traversal()?;
                let page = apply!(traverse(&request.relation, id, &traversal))?;
                Ok(json!({"items":page.items,"next_cursor":page.cursor}))
            }
            _ => Err(Error::Invalid),
        }
    }

    pub(crate) async fn read(
        &self,
        principal: &AuthenticatedPrincipal,
        uri: &str,
        tx: Option<&DataStoreTransaction>,
    ) -> Result<Value, Error> {
        let _fence = self.catalog.fence_data_store_commit().await;
        if !principal.revalidate_datastore_access().await {
            if let Some(tx) = tx {
                tx.mark_failed().await;
            }
            return Err(Error::NotFound);
        }
        if uri == ROOT {
            let snapshot = self.catalog.snapshot();
            return Ok(
                json!({"stores":snapshot.data_stores().iter().filter_map(|(name, store)|
                self.select(principal, name).ok().map(|_| json!({
                    "name":name,"uri":format!("{ROOT}/{}",encode(name)),
                    "type":store.definition().kind,"scope":store.definition().access.scope
                }))).collect::<Vec<_>>() }),
            );
        }
        let path = uri
            .strip_prefix(ROOT)
            .and_then(|tail| tail.strip_prefix('/'))
            .ok_or(Error::NotFound)?;
        let segments: Vec<_> = path.split('/').collect();
        let name = decode(segments[0])?;
        let selected = self.select(principal, &name)?;
        match segments.as_slice() {
            [_] => Ok(json!({"name":name,"spec":selected.definition})),
            [_, "objects", key] if tx.is_none() => {
                let key = decode(key)?;
                Ok(object_value(
                    &name,
                    self.objects.stat(&selected.namespace, &key).await?,
                ))
            }
            [_, "objects", key, "content"] if tx.is_none() => {
                use base64::{Engine as _, engine::general_purpose::STANDARD};
                use tokio::io::AsyncReadExt as _;
                let key = decode(key)?;
                let object = self.objects.read(&selected.namespace, &key).await?;
                if object.info.size > INLINE_BYTES as u64 {
                    return Err(Error::Limit);
                }
                let mut bytes = Vec::new();
                object
                    .body
                    .take(INLINE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|_| Error::Unavailable)?;
                if object.info.size != bytes.len() as u64 {
                    return Err(Error::Conflict);
                }
                Ok(
                    json!({"key":key,"mime_type":"application/octet-stream","content_base64":STANDARD.encode(bytes)}),
                )
            }
            [_, "collections", collection] => {
                let collection = decode(collection)?;
                Ok(
                    json!({"name":collection,"schema":selected.definition.collections.get(&collection).ok_or(Error::NotFound)?}),
                )
            }
            [_, "collections", collection, "records", id] => {
                let (collection, id) = (decode(collection)?, decode(id)?);
                let record = if let Some(tx) = tx {
                    let (selected, mut state) = self.enter(principal, &name, tx).await?;
                    state
                        .native
                        .as_mut()
                        .ok_or(Error::Unavailable)?
                        .get(&selected.namespace, &selected.definition, &collection, &id)
                        .await?
                } else {
                    self.relational
                        .get(&selected.namespace, &selected.definition, &collection, &id)
                        .await?
                };
                Ok(record_value(&name, &collection, record))
            }
            _ => Err(Error::NotFound),
        }
    }
}

const INLINE_BYTES: usize = 32 * 1024;

fn decode_inline(content: &str) -> Result<Vec<u8>, Error> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    if content.len() > INLINE_BYTES.div_ceil(3) * 4 {
        return Err(Error::Limit);
    }
    let bytes = STANDARD.decode(content).map_err(|_| Error::Invalid)?;
    if bytes.len() > INLINE_BYTES {
        return Err(Error::Limit);
    }
    Ok(bytes)
}

pub(crate) struct Storage {
    relational: relational::Relational,
    objects: object::Objects,
}

impl Storage {
    pub(crate) async fn local(root: &std::path::Path) -> Result<Self, Error> {
        if !root.is_absolute() {
            return Err(Error::Invalid);
        }
        tokio::fs::create_dir_all(root)
            .await
            .map_err(|_| Error::Unavailable)?;
        let relational =
            relational::Relational::open_sqlite(&root.join("datastore.sqlite3")).await?;
        let objects = object::Objects::filesystem(root.join("data-store-objects"))?;
        Ok(Self {
            relational,
            objects,
        })
    }

    pub(crate) async fn standalone(
        database: &crate::persistence::DatabaseConfig,
        workspace: &std::path::Path,
    ) -> Result<Self, Error> {
        use std::env::{VarError, var};
        let root = crate::persistence::standalone_data_directory().map_err(|_| Error::Invalid)?;
        let relational = match var("WALTR_DATASTORE_DATABASE_URL") {
            Ok(url) if url.starts_with("postgres://") || url.starts_with("postgresql://") => {
                relational::Relational::open_postgres(&url).await?
            }
            target => {
                let path = match target {
                    Ok(url) => {
                        let path = url.strip_prefix("sqlite:").ok_or(Error::Invalid)?;
                        let path = std::path::PathBuf::from(path);
                        if !path.is_absolute() || path.to_string_lossy().contains(['?', '#']) {
                            return Err(Error::Invalid);
                        }
                        path
                    }
                    Err(VarError::NotPresent) => root.join("datastore.sqlite3"),
                    Err(VarError::NotUnicode(_)) => return Err(Error::Invalid),
                };
                database
                    .validate_datastore_sqlite_path(&path)
                    .map_err(|_| Error::Invalid)?;
                Self::validate_workspace_path(workspace, &path)?;
                let parent = path.parent().ok_or(Error::Invalid)?;
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|_| Error::Unavailable)?;
                relational::Relational::open_sqlite(&path).await?
            }
        };
        let get = |name| var(name).map_err(|_| Error::Invalid);
        let objects = match var("WALTR_DATASTORE_OBJECT_STORE") {
            Ok(kind) if kind == "s3" => {
                let endpoint = url::Url::parse(&get("WALTR_DATASTORE_S3_ENDPOINT")?)
                    .map_err(|_| Error::Invalid)?;
                let loopback = endpoint.host_str().is_some_and(|host| {
                    host == "localhost"
                        || host
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                });
                if !(endpoint.scheme() == "https" || endpoint.scheme() == "http" && loopback)
                    || endpoint.host_str().is_none()
                    || !endpoint.username().is_empty()
                    || endpoint.password().is_some()
                    || endpoint.query().is_some()
                    || endpoint.fragment().is_some()
                    || endpoint.path() != "/"
                {
                    return Err(Error::Invalid);
                }
                let force_path_style = match get("WALTR_DATASTORE_S3_FORCE_PATH_STYLE")?.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => return Err(Error::Invalid),
                };
                let client = crate::session::storage::s3_client(
                    endpoint.to_string(),
                    get("WALTR_DATASTORE_S3_REGION")?,
                    get("WALTR_DATASTORE_S3_ACCESS_KEY_ID")?,
                    get("WALTR_DATASTORE_S3_SECRET_ACCESS_KEY")?,
                    force_path_style,
                );
                let replace_policy =
                    match var("WALTR_DATASTORE_S3_ALLOW_STALE_OVERWRITE").as_deref() {
                        Ok("true") => object::S3ReplacePolicy::AllowStaleOverwrite,
                        Ok("false") | Err(VarError::NotPresent) => {
                            object::S3ReplacePolicy::Conditional
                        }
                        _ => return Err(Error::Invalid),
                    };
                object::Objects::s3_with_replace_policy(
                    client,
                    get("WALTR_DATASTORE_S3_BUCKET")?,
                    get("WALTR_DATASTORE_S3_PREFIX")?,
                    replace_policy,
                )?
            }
            other => {
                if !matches!(&other, Err(VarError::NotPresent))
                    && other.as_deref() != Ok("filesystem")
                {
                    return Err(Error::Invalid);
                }
                let root = std::env::var_os("WALTR_DATASTORE_OBJECT_ROOT")
                    .map_or_else(|| root.join("data-store-objects"), std::path::PathBuf::from);
                Self::validate_workspace_path(workspace, &root)?;
                object::Objects::filesystem(root)?
            }
        };
        Ok(Self {
            relational,
            objects,
        })
    }

    pub(crate) fn validate_workspace_path(
        workspace: &std::path::Path,
        storage: &std::path::Path,
    ) -> Result<(), Error> {
        use crate::persistence::normalized_absolute_path;
        let workspace = normalized_absolute_path(workspace).map_err(|_| Error::Invalid)?;
        let storage = normalized_absolute_path(storage).map_err(|_| Error::Invalid)?;
        fn overlaps(path: &std::path::Path, root: &std::path::Path) -> Result<bool, Error> {
            if path.starts_with(root) {
                return Ok(true);
            }
            if !root.try_exists().map_err(|_| Error::Invalid)? {
                return Ok(false);
            }
            for ancestor in path.ancestors() {
                if ancestor.try_exists().map_err(|_| Error::Invalid)?
                    && same_file::is_same_file(root, ancestor).map_err(|_| Error::Invalid)?
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        if overlaps(&workspace, &storage)? || overlaps(&storage, &workspace)? {
            return Err(Error::Invalid);
        }
        Ok(())
    }

    pub(crate) fn bind(self, catalog: AgentDefinitionState) -> Arc<DataStores> {
        Arc::new(DataStores {
            catalog,
            relational: self.relational,
            objects: self.objects,
            transfers: Transfers::default(),
            admission: Arc::default(),
        })
    }
}
