use waltr_agent::{AgentDefinitionRegistry, ResolvedAgent};

#[allow(dead_code)]
pub fn filesystem_agent() -> ResolvedAgent {
    resolve_agent(
        r"spec:
  features: [{ featureSetRef: { name: test } }]
  permissions: [{ permissionSetRef: { name: test } }]
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: { name: test }
spec:
  filesystem:
    root: $(session.workspace.root)
    profile: auto
    defaultPermissions: true
---
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata: { name: test }
spec:
  tools:
    - { name: custom_search, permission: allow }",
    )
}

#[allow(dead_code)]
pub fn filesystem_mcp_agent() -> ResolvedAgent {
    resolve_agent(
        r"spec:
  features: [{ featureSetRef: { name: filesystem } }, { featureSetRef: { name: mcp } }]
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: { name: filesystem }
spec:
  filesystem:
    root: $(session.workspace.root)
    profile: auto
    defaultPermissions: true
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: { name: mcp }
spec:
  mcp: { exposeTools: true, instructions: true, resources: true, prompts: true, defaultPermissions: true }",
    )
}

#[allow(dead_code)]
pub fn task_agent(max_subagent_depth: usize) -> ResolvedAgent {
    resolve_agent(&format!(
        r"spec:
  features: [{{ featureSetRef: {{ name: test }} }}]
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: {{ name: test }}
spec:
  task: {{ maxSubagentDepth: {max_subagent_depth}, defaultPermissions: true }}"
    ))
}

#[allow(dead_code)]
pub fn live_task_visibility_agent(max_subagent_depth: usize, system_prompt: &str) -> ResolvedAgent {
    resolve_agent(&format!(
        r"spec:
  systemPrompt: {system_prompt:?}
  features: [{{ featureSetRef: {{ name: test }} }}]
  permissions: [{{ permissionSetRef: {{ name: test }} }}]
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: {{ name: test }}
spec:
  task: {{ maxSubagentDepth: {max_subagent_depth}, defaultPermissions: true }}
---
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata: {{ name: test }}
spec:
  tools: [{{ name: question, permission: allow }}]"
    ))
}

pub fn mcp_agent(instructions: bool) -> ResolvedAgent {
    mcp_agent_with_trace(instructions, None)
}

pub fn mcp_agent_with_trace(instructions: bool, trace_propagation: Option<bool>) -> ResolvedAgent {
    mcp_agent_with_options(instructions, trace_propagation, "")
}

#[allow(dead_code)]
pub fn mcp_agent_with_servers(servers: &str) -> ResolvedAgent {
    mcp_agent_with_options(true, None, &format!(", servers: {servers}"))
}

fn mcp_agent_with_options(
    instructions: bool,
    trace_propagation: Option<bool>,
    servers: &str,
) -> ResolvedAgent {
    let trace = trace_propagation
        .map(|enabled| format!(", tracePropagation: {enabled}"))
        .unwrap_or_default();
    resolve_agent(&format!(
        r"spec:
  features: [{{ featureSetRef: {{ name: test }} }}]
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: {{ name: test }}
spec:
  mcp: {{ exposeTools: true, instructions: {instructions}, resources: true, prompts: true, defaultPermissions: true{trace}{servers} }}"
    ))
}

fn resolve_agent(spec_and_resources: &str) -> ResolvedAgent {
    let source = format!(
        "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: test }}\n{spec_and_resources}\n"
    );
    let mut registry = AgentDefinitionRegistry::new();
    registry.load_yaml(&source).expect("load test definition");
    registry.resolve_agent("test").expect("resolve test agent")
}
