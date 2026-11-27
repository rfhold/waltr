use std::sync::Arc;

use mcp::progressive::{ActionInputSchema, input_schema, json_schema_for};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::access_auth::AuthenticatedPrincipal;
use crate::datastore::{self, DataStores};
use crate::permission::{PermissionLevel, PermissionPolicy};

use super::{
    Capability, ToolDefinition, dispatcher::CapabilityPolicy,
    middleware::mcp_resources::ResourceSelector,
};
use waltr_agent::{ToolExecutionContext, ToolResult};

pub(super) const SERVER: &str = "waltr";
pub(super) const GROUPS: [&str; 4] = ["query", "create", "execute", "destroy"];
const READ_IDENTITY: &str = "datastore:resources.read:read";

#[derive(Clone)]
pub(super) struct BoundDataStores {
    stores: Arc<DataStores>,
    principal: AuthenticatedPrincipal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    action: String,
    input: Value,
}

type Schema = (&'static str, fn() -> Value);

fn actions(group: &str) -> &'static [Schema] {
    use datastore::*;
    match group {
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
    }
}

fn identity(group: &str, action: &str) -> String {
    format!("datastore:{group}:{action}")
}

pub(super) fn local_resource(server: &str, uri: &str) -> bool {
    server == SERVER && datastore::is_resource(uri)
}

pub(super) fn group_alias(group: &str, occupied: &[String]) -> String {
    let usual = format!("{SERVER}__{group}");
    if !occupied.contains(&usual) {
        return usual;
    }
    let fallback = format!("{SERVER}__datastore_{group}");
    if !occupied.contains(&fallback) {
        return fallback;
    }
    (2..)
        .map(|i| format!("{fallback}_{i}"))
        .find(|alias| !occupied.contains(alias))
        .expect("finite tool aliases")
}

impl BoundDataStores {
    fn can_use(&self, group: &str, action: &str) -> bool {
        self.principal.datastore_actor().is_some()
            || crate::mcp_service::public_datastore_action_allowed(&self.principal, group, action)
    }

    pub(super) const fn new(stores: Arc<DataStores>, principal: AuthenticatedPrincipal) -> Self {
        Self { stores, principal }
    }

    pub(super) fn group(
        &self,
        group: &'static str,
        alias: String,
        policy: PermissionPolicy,
        authorizer: Arc<dyn CapabilityPolicy>,
    ) -> Option<Capability> {
        let actions: Vec<_> = actions(group)
            .iter()
            .filter(|(action, _)| {
                self.can_use(group, action)
                    && policy.evaluate_tool(&identity(group, action)) != PermissionLevel::Deny
            })
            .collect();
        if actions.is_empty() {
            return None;
        }
        let mut schema = input_schema(
            &actions
                .iter()
                .map(|(action, _)| *action)
                .collect::<Vec<_>>(),
            actions
                .into_iter()
                .map(|(action, schema)| ActionInputSchema {
                    action,
                    schema: Some(schema()),
                })
                .collect(),
        );
        schema["properties"]
            .as_object_mut()
            .expect("progressive schema properties")
            .remove("filter");
        let definition = ToolDefinition::new(
            alias,
            "First-party Waltr DataStore actions. Read waltr://data-stores with read_mcp_resource to discover accessible stores.",
            schema,
        );
        let bound = self.clone();
        Some(
            Capability::new_async(definition, move |context| {
                let bound = bound.clone();
                let authorizer = authorizer.clone();
                let policy = policy.clone();
                Box::pin(async move {
                    let Ok(args) =
                        serde_json::from_value::<Arguments>(context.call().arguments.clone())
                    else {
                        return Ok(context.result("invalid datastore arguments").with_error());
                    };
                    if !DataStores::handles(group, &args.action) {
                        return Ok(context.result("unknown datastore action").with_error());
                    }
                    if !bound.can_use(group, &args.action) {
                        return Ok(context
                            .result("datastore mutation requires an actor")
                            .with_error());
                    }
                    if let Err(message) = authorize(
                        &context,
                        &policy,
                        authorizer.as_ref(),
                        &identity(group, &args.action),
                    )
                    .await
                    {
                        return Ok(context.result(message).with_error());
                    }
                    let result = Box::pin(bound.stores.call(
                        &bound.principal, group, &args.action, args.input, None,
                    ))
                    .await;
                    Ok(tool_result(&context, result))
                })
            })
            .with_identity(format!("datastore:{group}")),
        )
    }

    pub(super) fn resource(
        &self,
        definition: ToolDefinition,
        policy: PermissionPolicy,
        authorizer: Arc<dyn CapabilityPolicy>,
    ) -> Capability {
        let bound = self.clone();
        Capability::new_async(definition, move |context| {
            let bound = bound.clone();
            let authorizer = authorizer.clone();
            let policy = policy.clone();
            Box::pin(async move {
                let Ok(ResourceSelector { server, uri }) =
                    serde_json::from_value(context.call().arguments.clone())
                else {
                    return Ok(context.result("invalid resource selector").with_error());
                };
                if !local_resource(&server, &uri) || uri.len() > 2048 {
                    return Ok(context.result("MCP resource is unavailable").with_error());
                }
                if let Err(message) =
                    authorize(&context, &policy, authorizer.as_ref(), READ_IDENTITY).await
                {
                    return Ok(context.result(message).with_error());
                }
                let result = bound.stores.read(&bound.principal, &uri, None).await.map(|value| {
                    json!({
                        "server": server, "uri": uri,
                        "data": {"contents": [{"uri": uri, "mimeType":"application/json", "text":value.to_string()}]},
                        "verified_skill_origins": []
                    })
                });
                Ok(tool_result(&context, result))
            })
        })
    }
}

async fn authorize(
    context: &ToolExecutionContext,
    policy: &PermissionPolicy,
    authorizer: &dyn CapabilityPolicy,
    identity: &str,
) -> Result<(), String> {
    // Alias admission happened in the dispatcher. Refine it against the final action and input.
    if policy.evaluate_tool(identity) == PermissionLevel::Deny
        || policy
            .evaluate_input(identity, &context.call().arguments)
            .is_some_and(|evaluation| {
                evaluation.effect.level != PermissionLevel::Allow
                    || evaluation.claims.iter().any(|claim| {
                        policy.evaluate_claim(identity, claim) != PermissionLevel::Allow
                    })
            })
    {
        return Err("datastore action denied by policy".into());
    }
    if let Some(message) = authorizer
        .authorize(context, identity, policy, &[])
        .await
        .map_err(|error| error.to_string())?
    {
        return Err(message);
    }
    if context.is_cancelled() {
        return Err("datastore operation cancelled".into());
    }
    context
        .check_execution()
        .await
        .map_err(|error| error.to_string())
}

fn tool_result(
    context: &ToolExecutionContext,
    result: Result<Value, waltr_datastore::Error>,
) -> ToolResult {
    match result {
        Ok(value) => context.result(value.to_string()),
        Err(error) => context.result(error.to_string()).with_error(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_host::dispatcher::{CapabilityDispatcher, PolicyFuture};
    use crate::agent_host::middleware::chain::MiddlewareChain;
    use crate::permission::{PermissionClaim, PermissionEffect, PermissionToolSelector};
    use waltr_agent::ToolCall;

    struct TestAuthorizer;

    impl CapabilityPolicy for TestAuthorizer {
        fn authorize<'a>(
            &'a self,
            context: &'a ToolExecutionContext,
            identity: &'a str,
            policy: &'a PermissionPolicy,
            _: &'a [PermissionClaim],
        ) -> PolicyFuture<'a> {
            Box::pin(async move {
                Ok((policy
                    .evaluate_target_tool_effect(identity, &context.call().name)
                    .level
                    == PermissionLevel::Deny)
                    .then(|| "denied".into()))
            })
        }
    }

    async fn dispatch(
        capability: Capability,
        policy: PermissionPolicy,
        arguments: Value,
    ) -> ToolResult {
        let name = capability.definition().name.clone();
        let dispatcher =
            CapabilityDispatcher::new(vec![capability], None, Arc::new(TestAuthorizer))
                .unwrap()
                .with_context(MiddlewareChain::new(), policy);
        let tool = dispatcher.agent_tools().remove(0);
        let call = ToolCall::new("call", name, arguments);
        let binding = tool.prepare_host_dispatch(&call).unwrap().unwrap();
        tool.call(ToolExecutionContext::new(call).with_host_operation(binding))
            .await
            .unwrap()
    }

    async fn fixture(dir: &std::path::Path) -> Arc<DataStores> {
        let resources = dir.join(".waltr/resources");
        std::fs::create_dir_all(resources.join("data-stores")).unwrap();
        std::fs::create_dir_all(resources.join("mcp-servers")).unwrap();
        std::fs::write(
            resources.join("mcp-servers/waltr.yaml"),
            "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {name: waltr}\nspec: {transport: streamableHttp, url: 'https://waltr.example', path: /mcp}\n",
        ).unwrap();
        for (name, scope) in [("notes", "shared"), ("private", "principal")] {
            std::fs::write(resources.join(format!("data-stores/{name}.yaml")), format!(
                "apiVersion: waltr.dev/v1alpha1\nkind: DataStore\nmetadata: {{name: {name}}}\nspec:\n  type: relational\n  access: {{scope: {scope}}}\n  collections:\n    note:\n      fields:\n        title: {{type: string, required: true}}\n",
            )).unwrap();
        }
        std::fs::write(
            resources.join("data-stores/files.yaml"),
            "apiVersion: waltr.dev/v1alpha1\nkind: DataStore\nmetadata: {name: files}\nspec: {type: object, access: {scope: shared}}\n",
        )
        .unwrap();
        let materializer = crate::catalog::CatalogMaterializer::load(
            &crate::test_support::default_catalog_loader(),
        )
        .await
        .unwrap();
        let catalog =
            crate::resource_catalog::AgentDefinitionState::load_unified(dir, materializer).unwrap();
        crate::datastore::Storage::local(dir)
            .await
            .unwrap()
            .bind(catalog)
    }

    #[tokio::test]
    async fn anonymous_datastore_agent_can_download_but_not_mutate() {
        let dir = tempfile::tempdir().unwrap();
        let stores = fixture(dir.path()).await;
        let path = dir.path().join("data-store-objects/object/files/shared");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("file"), b"external").unwrap();
        let bound = BoundDataStores::new(stores, AuthenticatedPrincipal::anonymous_disabled());
        let policy = PermissionPolicy::new(PermissionLevel::Allow);
        let authorizer = Arc::new(TestAuthorizer);
        for (group, available) in [("create", "object.download"), ("destroy", "object.close_transfer")] {
            let capability = bound
                .group(group, format!("waltr__{group}"), policy.clone(), authorizer.clone())
                .unwrap();
            assert_eq!(
                capability.definition().input_schema["properties"]["action"]["enum"],
                json!([available])
            );
            if group == "create" {
                let response = dispatch(
                    capability,
                    policy.clone(),
                    json!({"action":available,"input":{"store":"files","key":"file"}}),
                )
                .await;
                assert!(!response.is_error, "{}", response.content);
            }
        }
    }

    #[tokio::test]
    async fn dispatch_uses_the_submitter_for_attribution_and_private_resource_scope() {
        let dir = tempfile::tempdir().unwrap();
        let stores = fixture(dir.path()).await;
        let alice = AuthenticatedPrincipal::apps_test_principal("alice");
        let bob = AuthenticatedPrincipal::apps_test_principal("bob");
        let authorizer = Arc::new(TestAuthorizer);
        let policy = [
            "waltr__create",
            "waltr__execute",
            "read_mcp_resource",
            "datastore:create:record.create",
            "datastore:execute:record.update",
            READ_IDENTITY,
        ]
        .into_iter()
        .fold(
            PermissionPolicy::new(PermissionLevel::Deny),
            |policy, name| {
                policy.with_tool_rule(
                    PermissionToolSelector::exact(name),
                    PermissionEffect::allow(),
                )
            },
        );
        let alice_tools = BoundDataStores::new(stores.clone(), alice.clone());
        let bob_tools = BoundDataStores::new(stores.clone(), bob.clone());
        let create = alice_tools
            .group(
                "create",
                "waltr__create".into(),
                policy.clone(),
                authorizer.clone(),
            )
            .unwrap();
        assert_eq!(
            create.definition().input_schema["properties"]["action"]["enum"],
            json!(["record.create"])
        );
        let note = |store, title| json!({"store":store, "collection":"note", "id":"note-one", "fields":{"title":title}});
        let mut forged = note("notes", "forged");
        forged["actor"] = json!("bob");
        let result = dispatch(
            create.clone(),
            policy.clone(),
            json!({"action":"record.create","input":forged}),
        )
        .await;
        assert!(result.is_error);
        for store in ["notes", "private"] {
            let result = dispatch(
                create.clone(),
                policy.clone(),
                json!({"action":"record.create","input":note(store,"Alice")}),
            )
            .await;
            assert!(!result.is_error, "{}", result.content);
        }
        let update = bob_tools
            .group(
                "execute",
                "waltr__execute".into(),
                policy.clone(),
                authorizer.clone(),
            )
            .unwrap();
        let result = dispatch(
            update,
            policy.clone(),
            json!({"action":"record.update","input":note("notes","Bob")}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let shared = stores
            .read(
                &alice,
                "waltr://data-stores/notes/collections/note/records/note-one",
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            shared["attribution"]["created_by"],
            alice.datastore_actor().unwrap().as_str()
        );
        assert_eq!(
            shared["attribution"]["last_edited_by"],
            bob.datastore_actor().unwrap().as_str()
        );

        let resource =
            super::super::middleware::mcp_resources::McpResourcesMiddleware::definition(true);
        let read = json!({"server":"waltr","uri":"waltr://data-stores/private/collections/note/records/note-one"});
        let result = dispatch(
            bob_tools.resource(resource.clone(), policy.clone(), authorizer.clone()),
            policy.clone(),
            read.clone(),
        )
        .await;
        assert!(result.is_error);
        let result = dispatch(
            alice_tools.resource(resource, policy.clone(), authorizer.clone()),
            policy.clone(),
            read,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let envelope: Value = serde_json::from_str(&result.content.to_string()).unwrap();
        let record: Value =
            serde_json::from_str(envelope["data"]["contents"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(record["fields"]["title"], "Alice");
        let anonymous_tools =
            BoundDataStores::new(stores, AuthenticatedPrincipal::anonymous_disabled());
        assert!(
            anonymous_tools
                .group("create", "waltr__create".into(), policy, authorizer)
                .is_none()
        );
    }

    #[test]
    fn chooses_distinct_alias_and_reserves_only_local_resource_selector() {
        assert_eq!(group_alias("query", &[]), "waltr__query");
        let occupied = vec!["waltr__query".into(), "waltr__datastore_query".into()];
        assert_eq!(group_alias("query", &occupied), "waltr__datastore_query_2");
        assert!(local_resource("waltr", "waltr://data-stores"));
        assert!(local_resource("waltr", "waltr://data-stores/notes"));
        assert!(!local_resource("other", "waltr://data-stores"));
        assert!(!local_resource("waltr", "waltr://data-stores-other"));
    }

    #[test]
    fn every_declared_action_belongs_to_its_group() {
        for group in GROUPS {
            for (action, schema) in actions(group) {
                assert!(DataStores::handles(group, action));
                assert_eq!(schema()["type"], "object");
            }
        }
    }
}
