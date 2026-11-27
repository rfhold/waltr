use super::{ConcreteToolTarget, PortError, PortFuture};
use crate::{
    access_auth::AuthenticatedPrincipal,
    agent_host::authorized_tools::ConcreteToolSummary,
    datastore::{self, DataStoreTransaction, DataStores},
    permission::{PermissionLevel, PermissionPolicy},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
use waltr_datastore::Error as StoreError;
use waltr_script::{HostReply, HostRequest, Limits, StructuredValue};

const SERVER: &str = "waltr";
const TOOLS: [&str; 4] = ["query", "create", "execute", "destroy"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ToolArguments {
    target: ConcreteToolTarget,
    call_id: String,
    arguments: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionArguments {
    action: String,
    input: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceArguments {
    uri: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaArguments {
    targets: Vec<ConcreteToolTarget>,
}

enum Operation {
    Tool {
        target: ConcreteToolTarget,
        call_id: String,
        args: ActionArguments,
    },
    Read(ResourceArguments),
    Schemas(Vec<waltr_agent::ToolDefinition>),
}

fn is_relational_action(tool: &str, action: &str) -> bool {
    matches!(
        (tool, action),
        ("query", "record.find" | "relation.list")
            | ("create", "record.create" | "relation.link")
            | ("execute", "record.update")
            | ("destroy", "record.delete" | "relation.unlink")
    )
}

fn parse(request: &HostRequest) -> Result<Option<Operation>, PortError> {
    let input = request.arguments.as_json();
    let operation = match request.operation.as_str() {
        "tools.execute" => {
            let Some(target) = input.get("target") else {
                return Ok(None);
            };
            let tool = target.get("tool").and_then(Value::as_str);
            let action = input.pointer("/arguments/action").and_then(Value::as_str);
            let (Some(tool), Some(action)) = (tool, action) else {
                return Ok(None);
            };
            if target.get("server").and_then(Value::as_str) != Some(SERVER)
                || !DataStores::handles(tool, action)
            {
                return Ok(None);
            }
            let args: ToolArguments =
                serde_json::from_value(input.clone()).map_err(|_| PortError::Invalid)?;
            if args.call_id.is_empty() || args.call_id.len() > 256 {
                return Err(PortError::Invalid);
            }
            Operation::Tool {
                target: args.target,
                call_id: args.call_id,
                args: serde_json::from_value(args.arguments).map_err(|_| PortError::Invalid)?,
            }
        }
        "resources.read" => {
            let args: ResourceArguments =
                serde_json::from_value(input.clone()).map_err(|_| PortError::Invalid)?;
            if args.uri.len() > 2048 || !datastore::is_resource(&args.uri) {
                return Err(PortError::Denied);
            }
            Operation::Read(args)
        }
        _ => return Ok(None),
    };
    if request.callback.is_some() || request.handle.is_some() || request.bytes.is_some() {
        return Err(PortError::Invalid);
    }
    Ok(Some(operation))
}

pub(super) fn is_grouped_target(target: &ConcreteToolTarget) -> bool {
    target.server() == SERVER && TOOLS.contains(&target.tool())
}

fn actions(
    policy: &PermissionPolicy,
    tool: &str,
    relational: bool,
) -> Vec<(&'static str, fn() -> Value)> {
    use datastore::*;
    use mcp::progressive::json_schema_for;
    let all: &[(&str, fn() -> Value)] = match tool {
        "query" => &[
            ("object.list", json_schema_for::<ObjectList>),
            ("object.download_chunk", json_schema_for::<TransferHandle>),
            ("record.find", json_schema_for::<RecordFind>),
            ("relation.list", json_schema_for::<RelationList>),
        ],
        "create" => &[
            ("object.create", json_schema_for::<ObjectWrite>),
            ("object.upload", json_schema_for::<TransferOpen>),
            ("object.download", json_schema_for::<TransferOpen>),
            ("record.create", json_schema_for::<RecordCreate>),
            ("relation.link", json_schema_for::<Link>),
        ],
        "execute" => &[
            ("object.replace", json_schema_for::<ObjectWrite>),
            ("object.upload_chunk", json_schema_for::<TransferChunk>),
            ("record.update", json_schema_for::<RecordUpdate>),
        ],
        "destroy" => &[
            ("object.delete", json_schema_for::<ObjectKey>),
            ("object.close_transfer", json_schema_for::<TransferHandle>),
            ("record.delete", json_schema_for::<RecordKey>),
            ("relation.unlink", json_schema_for::<Link>),
        ],
        _ => &[],
    };
    all.iter()
        .copied()
        .filter(|(action, _)| {
            (!relational || is_relational_action(tool, action))
                && allows(policy, tool, action, &json!({"action":action}))
        })
        .collect()
}

pub(super) fn summaries(policy: &PermissionPolicy) -> Vec<ConcreteToolSummary> {
    TOOLS
        .into_iter()
        .filter_map(|tool| {
            (!actions(policy, tool, false).is_empty()).then(|| ConcreteToolSummary {
                target: ConcreteToolTarget::new(SERVER, tool).expect("bounded datastore identity"),
                description: "First-party datastore actions; inspect the selected schema.".into(),
            })
        })
        .collect()
}

pub(super) fn schema(
    policy: &PermissionPolicy,
    target: &ConcreteToolTarget,
    relational: bool,
) -> Result<waltr_agent::ToolDefinition, PortError> {
    let actions = actions(policy, target.tool(), relational);
    if !is_grouped_target(target) || actions.is_empty() {
        return Err(PortError::Denied);
    }
    let mut input_schema = mcp::progressive::input_schema(
        &actions
            .iter()
            .map(|(action, _)| *action)
            .collect::<Vec<_>>(),
        actions
            .iter()
            .map(|(action, schema)| mcp::progressive::ActionInputSchema {
                action,
                schema: Some(schema()),
            })
            .collect(),
    );
    // Code already has a bounded JavaScript projection path.
    input_schema["properties"]
        .as_object_mut()
        .expect("progressive schema properties")
        .remove("filter");
    Ok(waltr_agent::ToolDefinition {
        name: target.tool().into(),
        description: "First-party datastore actions.".into(),
        input_schema,
    })
}

// A first-party permission identity cannot be made into an upstream target by configuration.
pub(super) fn allows(
    policy: &PermissionPolicy,
    tool: &str,
    action: &str,
    arguments: &Value,
) -> bool {
    let identity = format!("datastore:{tool}:{action}");
    policy.evaluate_target_tool_effect(&identity, tool).level == PermissionLevel::Allow
        && [tool, identity.as_str()].into_iter().all(|name| {
            policy
                .evaluate_input(name, arguments)
                .is_none_or(|evaluation| {
                    evaluation.effect.level == PermissionLevel::Allow
                        && evaluation.claims.iter().all(|claim| {
                            policy.evaluate_claim(name, claim) == PermissionLevel::Allow
                        })
                })
        })
}

pub(super) struct CodeDataStores {
    stores: Arc<DataStores>,
    principal: AuthenticatedPrincipal,
    policy: PermissionPolicy,
    limits: Limits,
    public: bool,
    call_ids: Mutex<BTreeSet<String>>,
    pub(super) transaction: Option<Arc<CodeTransaction>>,
}

impl CodeDataStores {
    pub(super) fn new(
        stores: Arc<DataStores>,
        principal: AuthenticatedPrincipal,
        policy: PermissionPolicy,
        limits: Limits,
        relational: bool,
        public: bool,
    ) -> Self {
        Self {
            transaction: relational.then(|| Arc::new(CodeTransaction::new(stores.begin()))),
            stores,
            principal,
            policy,
            limits,
            public,
            call_ids: Mutex::default(),
        }
    }

    fn operation(&self, request: &HostRequest) -> Result<Option<Operation>, PortError> {
        if request.operation == "tools.schemas" {
            let args: SchemaArguments = serde_json::from_value(request.arguments.as_json().clone())
                .map_err(|_| PortError::Invalid)?;
            if args.targets.iter().any(is_grouped_target) {
                if args.targets.len() > 8
                    || request.callback.is_some()
                    || request.handle.is_some()
                    || request.bytes.is_some()
                {
                    return Err(PortError::Invalid);
                }
                // Keep local schema discovery independent of any upstream round trip.
                return args
                    .targets
                    .iter()
                    .map(|target| schema(&self.policy, target, self.transaction.is_some()))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|schemas| Some(Operation::Schemas(schemas)));
            }
        }
        parse(request).and_then(|operation| {
            let allowed = match operation.as_ref() {
                Some(Operation::Tool { target, args, .. }) => {
                    (self.transaction.is_none()
                        || is_relational_action(target.tool(), &args.action))
                        && (!self.public
                            || crate::mcp_service::public_datastore_action_allowed(
                                &self.principal,
                                target.tool(),
                                &args.action,
                            ))
                        && allows(
                            &self.policy,
                            target.tool(),
                            &args.action,
                            request
                                .arguments
                                .as_json()
                                .get("arguments")
                                .expect("parsed tool arguments"),
                        )
                }
                Some(Operation::Read(args)) => {
                    let object = args
                        .uri
                        .strip_prefix(datastore::ROOT)
                        .and_then(|path| path.strip_prefix('/'))
                        .and_then(|path| path.split('/').nth(1))
                        == Some("objects");
                    (self.transaction.is_none() || !object)
                        && allows(
                            &self.policy,
                            "resources.read",
                            "read",
                            &json!({"uri":args.uri}),
                        )
                }
                Some(Operation::Schemas(_)) => unreachable!("schemas are handled above"),
                None => self.transaction.is_none(),
            };
            allowed.then_some(operation).ok_or(PortError::Denied)
        })
    }

    pub(super) fn before(&self, request: &HostRequest) -> Option<PortFuture<Value>> {
        let operation = self.operation(request).transpose()?;
        let transaction = self.transaction.clone();
        let taint_denial = request.operation != "resources.read";
        Some(Box::pin(async move {
            if operation.is_err()
                && taint_denial
                && let Some(transaction) = &transaction
            {
                transaction.fail("denied").await;
            }
            let operation = operation?;
            let mut projection = match operation {
                Operation::Tool {
                    target,
                    call_id,
                    args,
                } => {
                    json!({"target": target, "action": args.action, "callId": call_id})
                }
                Operation::Read(_) => json!({"operation":"resources.read"}),
                Operation::Schemas(_) => json!({"operation":"tools.schemas"}),
            };
            if transaction.is_some() {
                projection["provisional"] = Value::Bool(true);
            }
            Ok(projection)
        }))
    }

    pub(super) async fn dispatch(
        &self,
        request: &HostRequest,
    ) -> Result<Option<HostReply>, waltr_script::Error> {
        let Some(operation) = self
            .operation(request)
            .map_err(|_| waltr_script::Error::Denied)?
        else {
            return Ok(None);
        };
        let transaction = self.transaction.as_ref();
        let sql = transaction.map(|transaction| &transaction.inner);
        // Refuse additional datastore work once a prior attempted effect has failed.
        if transaction.is_some_and(|transaction| transaction.error().is_some()) {
            return self
                .reply(json!({"status":"failed","error":"rollback_only"}))
                .map(Some);
        }
        let (result, mutation) = match operation {
            Operation::Tool {
                target,
                call_id,
                args,
            } => {
                let tool = target.tool();
                let fresh = self
                    .call_ids
                    .lock()
                    .map_err(|_| waltr_script::Error::Cleanup)?
                    .insert(call_id);
                if !fresh {
                    if tool != "query"
                        && let Some(transaction) = transaction
                    {
                        transaction.fail("conflict").await;
                    }
                    return self
                        .reply(json!({"status":"failed","error":"conflict"}))
                        .map(Some);
                }
                (
                    self.stores
                        .call(&self.principal, tool, &args.action, args.input, sql)
                        .await,
                    tool != "query",
                )
            }
            Operation::Read(args) => (
                self.stores.read(&self.principal, &args.uri, sql).await,
                false,
            ),
            Operation::Schemas(schemas) => {
                return self
                    .reply(json!({"status":"completed","schemas":schemas}))
                    .map(Some);
            }
        };
        let reply = match result {
            Ok(result) => self.reply(json!({"status":"completed","result":result})),
            Err(StoreError::Uncertain) if transaction.is_none() => {
                return Err(waltr_script::Error::Cleanup);
            }
            Err(error) => {
                let category = category(error);
                if mutation && let Some(transaction) = transaction {
                    transaction.fail(category).await;
                }
                self.reply(json!({"status":"failed","error":category}))
            }
        };
        if reply.is_err()
            && mutation
            && let Some(transaction) = transaction
        {
            transaction.fail("result_limit").await;
        }
        reply.map(Some)
    }

    fn reply(&self, mut value: Value) -> Result<HostReply, waltr_script::Error> {
        value["datastore"] = Value::Bool(true);
        StructuredValue::new(value, &self.limits).map(HostReply::Value)
    }
}

pub(super) fn project_result(reply: &HostReply) -> Option<Value> {
    let HostReply::Value(value) = reply else {
        return None;
    };
    let value = value.as_json();
    if value.get("datastore") != Some(&Value::Bool(true)) {
        return None;
    }
    Some(json!({"status": value.get("status"), "error": value.get("error")}))
}

pub(super) struct CodeTransaction {
    inner: DataStoreTransaction,
    error: Mutex<Option<&'static str>>,
}

impl CodeTransaction {
    fn new(inner: DataStoreTransaction) -> Self {
        Self {
            inner,
            error: Mutex::default(),
        }
    }

    pub(super) fn error(&self) -> Option<&'static str> {
        *self.error.lock().expect("code transaction error mutex")
    }

    async fn fail(&self, category: &'static str) {
        self.error
            .lock()
            .expect("code transaction error mutex")
            .get_or_insert(category);
        self.inner.mark_failed().await;
    }

    pub(super) async fn finish(&self, invocation: &str, commit: bool) -> Result<(), PortError> {
        let rollback_only = self.error().is_some();
        if let Err(error) = self
            .inner
            .finish(invocation, commit && !rollback_only)
            .await
        {
            let port = match error {
                StoreError::Conflict => PortError::Conflict,
                StoreError::Invalid | StoreError::Limit | StoreError::NotFound => {
                    PortError::Invalid
                }
                _ => PortError::Uncertain,
            };
            self.error
                .lock()
                .expect("code transaction error mutex")
                .get_or_insert(category(error));
            return Err(port);
        }
        if commit && rollback_only {
            return Err(PortError::Conflict);
        }
        Ok(())
    }
}

fn category(error: StoreError) -> &'static str {
    match error {
        StoreError::Invalid => "invalid",
        StoreError::NotFound => "not_found",
        StoreError::Conflict => "conflict",
        StoreError::Limit => "limit",
        StoreError::Unavailable => "unavailable",
        StoreError::Uncertain => "uncertain",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{PermissionEffect, PermissionToolSelector};

    fn tool(server: &str, tool: &str, action: &str) -> HostRequest {
        HostRequest {
            operation: "tools.execute".into(),
            arguments: StructuredValue::new(
                json!({"target":{"server":server,"tool":tool},"callId":"test","arguments":{"action":action,"input":{}}}),
                &Limits::default(),
            )
            .unwrap(),
            bytes: None,
            callback: None,
            handle: None,
        }
    }

    #[test]
    fn local_dispatch_requires_the_exact_server_and_action_pair() {
        assert!(matches!(
            parse(&tool("waltr", "create", "record.create")),
            Ok(Some(Operation::Tool { .. }))
        ));
        assert!(matches!(
            parse(&tool("other", "create", "record.create")),
            Ok(None)
        ));
        assert!(matches!(
            parse(&tool("waltr", "query", "record.create")),
            Ok(None)
        ));
        assert!(matches!(
            parse(&tool("waltr", "create", "session.create")),
            Ok(None)
        ));
        assert!(!is_relational_action("create", "object.create"));
    }

    #[test]
    fn datastore_permissions_intersect_and_cannot_request_consent() {
        let args = json!({"action":"record.create","input":{"store":"notes"}});
        let allow = PermissionPolicy::new(PermissionLevel::Allow);
        assert!(allows(&allow, "create", "record.create", &args));
        for level in [PermissionLevel::Ask, PermissionLevel::Deny] {
            let restricted = PermissionPolicy::new(PermissionLevel::Allow).with_tool_rule(
                PermissionToolSelector::exact("datastore:create:record.create"),
                PermissionEffect::new(level),
            );
            let limited = allow.clone().intersection(restricted);
            assert!(!allows(&limited, "create", "record.create", &args));
            assert!(allows(&limited, "create", "relation.link", &args));
        }
    }

    #[test]
    fn transfer_actions_route_locally_but_not_in_relational_mode() {
        let policy = PermissionPolicy::new(PermissionLevel::Allow);
        for (group, action) in [
            ("create", "object.upload"),
            ("create", "object.download"),
            ("execute", "object.upload_chunk"),
            ("query", "object.download_chunk"),
            ("destroy", "object.close_transfer"),
        ] {
            assert!(matches!(
                parse(&tool("waltr", group, action)),
                Ok(Some(Operation::Tool { .. }))
            ));
            assert!(
                actions(&policy, group, false)
                    .iter()
                    .any(|(name, _)| *name == action)
            );
            assert!(
                !actions(&policy, group, true)
                    .iter()
                    .any(|(name, _)| *name == action)
            );
        }
    }
}
