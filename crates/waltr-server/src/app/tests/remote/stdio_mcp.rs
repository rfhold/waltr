use super::*;
use waltr::grpc::waltr::{routine::v2 as p, session::v1::AgentSelectionRequest};

pub(super) const SERVER: &str = "workspace-marker";
const TOOL: &str = "read.marker";
const AGENT: &str = "workspace-marker-source";
const ROUTINE: &str = "read-workspace-marker";
const PROGRAM: &str = "marker-mcp.sh";

pub(super) enum Role {
    Destination,
    Exporter,
}

pub(super) fn install_program(directory: &Path) {
    std::fs::write(directory.join(PROGRAM), include_str!("marker-mcp.sh")).unwrap();
}

pub(super) fn install_resources(root: &Path, role: Role) -> RoutineAuthority {
    let resources = root.join(".waltr/resources");
    let launch_arg = match role {
        Role::Destination => "destination",
        Role::Exporter => "exporter",
    };
    std::fs::write(
        resources.join(format!("mcp-servers/{SERVER}.yaml")),
        format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {{name: {SERVER}}}\nspec: {{transport: stdio, command: '/bin/sh ./{PROGRAM} {launch_arg}'}}\n"
        ),
    )
    .unwrap();
    if matches!(role, Role::Exporter) {
        return RoutineAuthority::default();
    }
    std::fs::create_dir_all(resources.join("agents")).unwrap();
    std::fs::write(
        resources.join(format!("agents/{AGENT}.yaml")),
        format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{name: {AGENT}}}\nspec:\n  features:\n    - mcp: {{servers: {{include: [{SERVER}]}}, defaultPermissions: false}}\n  permissions: [{{feature: mcp, permission: allow}}]\n"
        ),
    )
    .unwrap();
    let source = format!(
        r#"export default async function () {{
            const reply = await host.call('tools.execute', {{
                target: {{server: '{SERVER}', tool: '{TOOL}'}}, arguments: {{}},
                workspace: {{machine: '{MACHINE}', workingDirectory: '{PROJECT}'}}
            }});
            if (reply.status !== 'completed' || reply.result.is_error) throw Error('marker call failed');
            return reply.result.content[0].text;
        }}"#
    );
    std::fs::create_dir_all(resources.join("routines")).unwrap();
    std::fs::write(
        resources.join(format!("routines/{ROUTINE}.yaml")),
        serde_json::to_vec(&json!({
            "apiVersion": "waltr.routine/v1",
            "kind": "Routine",
            "name": ROUTINE,
            "description": "Read the marker in the exporting worker's selected directory",
            "spec": {
                "source": source,
                "entrypoints": {"run": {"trigger": "manual"}},
                "parameters": {"schemaVersion": "waltr.value/v1", "schema": {"type": "null"}},
                "result": {"schemaVersion": "waltr.value/v1", "schema": {"type": "string", "maxLength": 128}},
                "capabilities": ["tools.execute"],
                "mcpTools": [{"server":SERVER,"tool":TOOL}],
            },
        }))
        .unwrap(),
    )
    .unwrap();
    RoutineAuthority::default()
}

pub(super) struct Probe {
    routines: p::routine_service_client::RoutineServiceClient<Channel>,
    revision: String,
    session: String,
}

impl Probe {
    pub(super) async fn new(
        channel: Channel,
        sessions: &mut SessionServiceClient<Channel>,
        bearer: &str,
    ) -> Self {
        let mut create = selection(MACHINE, bearer);
        create.get_mut().agent_selection = Some(AgentSelectionRequest {
            agent_name: AGENT.into(),
        });
        let session = sessions
            .create_session(create)
            .await
            .unwrap()
            .into_inner()
            .session
            .unwrap();
        let workspace = session.workspace.unwrap();
        assert_eq!(workspace.machine_id, MACHINE);
        assert_eq!(workspace.working_directory.as_deref(), Some(PROJECT));

        let mut routines = p::routine_service_client::RoutineServiceClient::new(channel);
        let first = routines
            .watch_definitions(authenticated(p::WatchDefinitionsRequest {}, bearer))
            .await
            .unwrap()
            .into_inner()
            .message()
            .await
            .unwrap()
            .unwrap();
        let definition = first
            .definitions
            .into_iter()
            .find(|definition| definition.name == ROUTINE)
            .expect("destination Routine should be published");
        Self {
            routines,
            revision: definition.revision,
            session: session.id,
        }
    }

    async fn start(&self, bearer: &str) -> Result<p::Invocation, Status> {
        let response = self
            .routines
            .clone()
            .start_invocation(authenticated(
                p::StartInvocationRequest {
                    selection: Some(p::InvocationSelection {
                        routine: ROUTINE.into(),
                        revision: self.revision.clone(),
                        entrypoint: "run".into(),
                        source_session: Some(self.session.clone()),
                        parameters: Some(p::StructuredValue {
                            json: b"null".to_vec(),
                        }),
                        ..Default::default()
                    }),
                },
                bearer,
            ))
            .await?;
        Ok(response.into_inner().invocation.unwrap())
    }

    pub(super) async fn assert_denied(&self, bearer: &str) {
        assert_eq!(
            self.start(bearer).await.unwrap_err().code(),
            Code::PermissionDenied
        );
    }

    pub(super) async fn assert_marker(&self, bearer: &str) {
        let id = self.start(bearer).await.unwrap().id;
        let mut routines = self.routines.clone();
        let mut watch = routines
            .watch_invocations(authenticated(
                p::WatchInvocationsRequest {
                    source_session: Some(self.session.clone()),
                    limit: 10,
                    ..Default::default()
                },
                bearer,
            ))
            .await
            .unwrap()
            .into_inner();
        while let Some(update) = watch.message().await.unwrap() {
            if update
                .invocations
                .iter()
                .any(|row| row.id == id && row.status >= p::InvocationStatus::Succeeded as i32)
            {
                let row = routines
                    .get_invocation(authenticated(
                        p::GetInvocationRequest { invocation_id: id },
                        bearer,
                    ))
                    .await
                    .unwrap()
                    .into_inner()
                    .invocation
                    .unwrap();
                assert_eq!(row.status, p::InvocationStatus::Succeeded as i32, "{row:?}");
                let marker: String = serde_json::from_slice(&row.result.unwrap().json).unwrap();
                assert_eq!(marker, CONTENTS);
                return;
            }
        }
        panic!("marker invocation stream closed before completion");
    }
}
