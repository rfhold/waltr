use std::{collections::BTreeSet, path::PathBuf};

use crate::{
    AgentDefinitionResourceKey, PermissionCommandPattern, PermissionLevel, PermissionPathPattern,
    definition::{
        AgentDefinitionBundle, AgentDefinitionCatalog, AgentDefinitionError,
        AgentDefinitionRegistry, AgentDefinitionResourceKind, AgentFeature,
        AgentPermissionSelector, FilesystemFeatureProfile, McpServerSelection, ResolvedPermission,
        SESSION_WORKSPACE_ROOT,
    },
    inspect_agent_definition_source,
};

const DEFAULT_AGENT: &str = "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: default }\nspec:\n  description: Provide the shared base behavior for Waltr agents.\n  systemPrompt: >-\n    You are Waltr Agent, a careful coding and operations assistant. Follow the user's instructions, use available tools when they are helpful, and report results clearly. Treat tool errors as recoverable information and continue when possible.\n  permissions:\n    - permissionSetRef: { name: default }\n";
const CODING_AGENT: &str = "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: coding }\nspec:\n  description: Provide the complete coding behavior currently assembled by the server.\n  inheritsFrom: { name: default }\n  features:\n    - featureSetRef: { name: coding }\n  permissions:\n    - permissionSetRef: { name: coding }\n";
const CODING_FEATURES: &str = r"apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata:
  name: coding
spec:
  filesystem:
    root: $(session.workspace.root)
    profile: auto
    systemPrompt: true
    agentsReadContext: true
    skillsContext: true
    defaultPermissions: true
  agents:
    root: $(session.workspace.root)
    filename: AGENTS.md
    systemPrompt: true
  skills:
    root: $(session.workspace.root)
    defaultPermissions: true
  shell:
    root: $(session.workspace.root)
    systemPrompt: true
    defaultPermissions: true
  task:
    maxSubagentDepth: 1
    defaultPermissions: true
  mcp:
    servers:
      exclude: []
    exposeTools: true
    instructions: true
    resources: true
    prompts: true
    tracePropagation: true
    defaultPermissions: true
";
const DEFAULT_PERMISSIONS: &str = r"apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: default
spec:
  tools:
    - name: question
      permission: allow
";
const CODING_PERMISSIONS: &str = r"apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: coding
spec:
  tools:
    - feature: filesystem
      permission:
        resources:
          paths:
            - match:
                outsideRoot: $(session.workspace.root)
              effect: ask
    - name: shell
      permission:
        resources:
          paths:
            - match:
                outsideRoot: $(session.workspace.root)
              effect: ask
          commands:
            - match:
                names:
                  - git
                  - pulumi
                  - kubectl
                  - terraform
                  - gcloud
                  - aws
                  - gh
                  - help
                  - pyinfra
                  - ansible
                  - sudo
                  - ssh
              effect: ask
";

#[test]
fn source_inspection_returns_resource_keys_in_document_order() {
    let keys = inspect_agent_definition_source(
        r"
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata: { name: permissions }
spec: { tools: [] }
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: agent }
spec: {}
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: { name: features }
spec: {}
",
    )
    .unwrap();

    assert_eq!(
        keys,
        vec![
            AgentDefinitionResourceKey {
                kind: AgentDefinitionResourceKind::PermissionSet,
                name: "permissions".to_owned(),
            },
            AgentDefinitionResourceKey {
                kind: AgentDefinitionResourceKind::Agent,
                name: "agent".to_owned(),
            },
            AgentDefinitionResourceKey {
                kind: AgentDefinitionResourceKind::FeatureSet,
                name: "features".to_owned(),
            },
        ]
    );
}

#[test]
fn source_inspection_preserves_strict_validation_and_duplicate_errors() {
    let invalid = inspect_agent_definition_source(
        "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: invalid }\nspec: { model: unsupported }\n",
    )
    .unwrap_err();
    assert!(matches!(
        invalid,
        AgentDefinitionError::Yaml { document: 1, .. }
    ));

    let duplicate = inspect_agent_definition_source(
        "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: duplicate }\nspec: {}\n---\napiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: duplicate }\nspec: {}\n",
    )
    .unwrap_err();
    assert!(matches!(
        duplicate,
        AgentDefinitionError::DuplicateResource {
            document: 2,
            kind: AgentDefinitionResourceKind::Agent,
            ref name,
        } if name == "duplicate"
    ));
}

#[test]
fn durable_bundle_reloads_sources_in_order_and_resolves_selection() {
    let bundle = AgentDefinitionBundle::new(
        "selected",
        vec![
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: selected\nspec:\n  description: first\n".to_owned(),
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: selected\nspec:\n  description: second\n".to_owned(),
        ],
    );

    let serialized = serde_json::to_string(&bundle).unwrap();
    let restored: AgentDefinitionBundle = serde_json::from_str(&serialized).unwrap();
    let resolved = restored.resolve().unwrap();

    assert_eq!(restored, bundle);
    assert_eq!(resolved.name, "selected");
    assert_eq!(resolved.description.as_deref(), Some("second"));
}

#[test]
fn catalog_is_sorted_selects_default_and_builds_exact_bundles() {
    let sources = vec![
        "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: zeta }\nspec: { description: last }\n---\napiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: alpha }\nspec: { description: first }\n"
            .to_owned(),
    ];
    let catalog = AgentDefinitionCatalog::from_sources(sources.clone(), "zeta").unwrap();

    assert_eq!(catalog.default_agent_name(), "zeta");
    assert_eq!(
        catalog
            .entries()
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
    assert_eq!(catalog.bundle("alpha").unwrap().sources, sources);
    assert_eq!(catalog.default_bundle().agent_name, "zeta");
    assert!(catalog.bundle("missing").is_none());
}

#[test]
fn bundle_identity_is_stable_and_sensitive_without_exposing_source() {
    let first = AgentDefinitionBundle::new("alpha", vec!["secret prompt".to_owned()]);
    let same = AgentDefinitionBundle::new("alpha", vec!["secret prompt".to_owned()]);
    let changed = AgentDefinitionBundle::new("alpha", vec!["changed prompt".to_owned()]);

    assert_eq!(first.identity(), same.identity());
    assert_ne!(first.identity(), changed.identity());
    assert_eq!(first.identity().len(), 64);
    assert!(!first.identity().contains("secret"));
}

#[test]
fn durable_bundle_surfaces_malformed_sources_and_missing_selection() {
    let malformed = AgentDefinitionBundle::new("selected", vec!["kind: [".to_owned()]);
    assert!(matches!(
        malformed.resolve(),
        Err(AgentDefinitionError::Yaml { .. })
    ));

    let missing = AgentDefinitionBundle::new("selected", Vec::new());
    assert!(matches!(
        missing.resolve(),
        Err(AgentDefinitionError::MissingReference {
            kind: AgentDefinitionResourceKind::Agent,
            ref name,
            field: None,
        }) if name == "selected"
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn resolves_proposed_coding_fixtures() {
    let mut registry = AgentDefinitionRegistry::new();
    for source in [
        DEFAULT_AGENT,
        CODING_AGENT,
        CODING_FEATURES,
        DEFAULT_PERMISSIONS,
        CODING_PERMISSIONS,
    ] {
        registry.load_yaml(source).expect("fixture should parse");
    }

    let resolved = registry
        .resolve_agent("coding")
        .expect("coding agent should resolve");

    assert_eq!(resolved.name, "coding");
    assert_eq!(
        resolved.description.as_deref(),
        Some("Provide the complete coding behavior currently assembled by the server.")
    );
    assert_eq!(
        resolved.system_prompt.as_deref(),
        Some(
            "You are Waltr Agent, a careful coding and operations assistant. Follow the user's instructions, use available tools when they are helpful, and report results clearly. Treat tool errors as recoverable information and continue when possible."
        )
    );

    let filesystem = resolved
        .features
        .filesystem
        .as_ref()
        .expect("filesystem should be enabled");
    assert_eq!(filesystem.root, SESSION_WORKSPACE_ROOT);
    assert_eq!(filesystem.profile, FilesystemFeatureProfile::Auto);
    assert!(filesystem.system_prompt);
    assert!(filesystem.agents_read_context);
    assert!(filesystem.skills_context);
    assert!(filesystem.default_permissions);

    let agents = resolved
        .features
        .agents
        .as_ref()
        .expect("agents should be enabled");
    assert_eq!(agents.root, SESSION_WORKSPACE_ROOT);
    assert_eq!(agents.filename, "AGENTS.md");
    assert!(agents.system_prompt);

    assert!(
        resolved
            .features
            .skills
            .as_ref()
            .unwrap()
            .default_permissions
    );
    assert!(resolved.features.shell.as_ref().unwrap().system_prompt);
    assert_eq!(
        resolved.features.task.as_ref().unwrap().max_subagent_depth,
        1
    );
    let mcp = resolved
        .features
        .mcp
        .as_ref()
        .expect("mcp should be enabled");
    assert!(mcp.expose_tools);
    assert_eq!(mcp.servers, McpServerSelection::default());
    assert!(mcp.instructions);
    assert!(mcp.resources);
    assert!(mcp.prompts);
    assert!(mcp.trace_propagation);
    assert!(mcp.default_permissions);

    assert_eq!(resolved.permissions.len(), 3);
    assert!(matches!(
        &resolved.permissions[0].selector,
        AgentPermissionSelector::Name(name) if name == "question"
    ));
    assert!(matches!(
        &resolved.permissions[0].permission,
        ResolvedPermission::Tool(effect) if effect.level == PermissionLevel::Allow
    ));
    assert_eq!(
        resolved.permissions[1].selector,
        AgentPermissionSelector::Feature(AgentFeature::Filesystem)
    );
    let ResolvedPermission::Resources(filesystem_permissions) = &resolved.permissions[1].permission
    else {
        panic!("filesystem permission should contain resource rules");
    };
    assert_eq!(filesystem_permissions.paths.len(), 1);
    assert_eq!(
        filesystem_permissions.paths[0].pattern,
        PermissionPathPattern::OutsideRoot(PathBuf::from(SESSION_WORKSPACE_ROOT))
    );
    assert_eq!(
        filesystem_permissions.paths[0].effect.level,
        PermissionLevel::Ask
    );

    let ResolvedPermission::Resources(shell_permissions) = &resolved.permissions[2].permission
    else {
        panic!("shell permission should contain resource rules");
    };
    assert_eq!(shell_permissions.paths.len(), 1);
    assert_eq!(shell_permissions.commands.len(), 12);
    assert_eq!(
        shell_permissions.commands[0].pattern,
        PermissionCommandPattern::Name("git".to_owned())
    );
    assert_eq!(
        shell_permissions.commands[11].pattern,
        PermissionCommandPattern::Name("ssh".to_owned())
    );
    assert!(
        shell_permissions
            .commands
            .iter()
            .all(|rule| rule.effect.level == PermissionLevel::Ask)
    );
}

#[test]
fn mcp_server_selection_accepts_include_exclude_and_omission() {
    for (servers, expected) in [
        (
            "servers:\n      include: [zeta, alpha]",
            McpServerSelection::Include(BTreeSet::from(["alpha".to_owned(), "zeta".to_owned()])),
        ),
        (
            "servers:\n      include: []",
            McpServerSelection::Include(BTreeSet::new()),
        ),
        (
            "servers:\n      exclude: [zeta, alpha]",
            McpServerSelection::Exclude(BTreeSet::from(["alpha".to_owned(), "zeta".to_owned()])),
        ),
        (
            "servers:\n      exclude: []",
            McpServerSelection::Exclude(BTreeSet::new()),
        ),
        ("{}", McpServerSelection::default()),
    ] {
        let source = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: FeatureSet\nmetadata:\n  name: selected\nspec:\n  mcp:\n    {servers}\n"
        );
        let resolved = resolve_mcp_feature(&source);
        assert_eq!(resolved.servers, expected, "source should parse:\n{source}");
        assert!(resolved.trace_propagation);
    }
}

#[test]
fn mcp_server_selection_rejects_invalid_shapes_and_names() {
    let invalid = [
        "servers: {}",
        "servers: null",
        "servers:\n      include: []\n      exclude: []",
        "servers:\n      unknown: []",
        "servers:\n      include: ['']",
        "servers:\n      exclude: ['   ']",
        "servers:\n      include: [same, same]",
        "servers:\n      exclude: [same, same]",
        "servers:\n      include: []\n      include: []",
        "servers:\n      exclude: []\n      exclude: []",
    ];

    for servers in invalid {
        let source = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: FeatureSet\nmetadata:\n  name: invalid\nspec:\n  mcp:\n    {servers}\n"
        );
        assert!(
            AgentDefinitionRegistry::new().load_yaml(&source).is_err(),
            "source should fail strict parsing:\n{source}"
        );
    }
}

#[test]
fn mcp_trace_propagation_defaults_true_and_preserves_explicit_false() {
    let omitted = resolve_mcp_feature(
        "apiVersion: waltr.dev/v1alpha1\nkind: FeatureSet\nmetadata:\n  name: selected\nspec:\n  mcp: {}\n",
    );
    let disabled = resolve_mcp_feature(
        "apiVersion: waltr.dev/v1alpha1\nkind: FeatureSet\nmetadata:\n  name: selected\nspec:\n  mcp:\n    tracePropagation: false\n",
    );

    assert!(omitted.trace_propagation);
    assert!(!disabled.trace_propagation);
}

#[test]
fn later_feature_set_replaces_mcp_selector_and_trace_together() {
    let mut registry = AgentDefinitionRegistry::new();
    registry
        .load_yaml(
            r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: selected
spec:
  features:
    - featureSetRef: { name: first }
    - featureSetRef: { name: second }
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata:
  name: first
spec:
  mcp:
    servers:
      include: [first]
    tracePropagation: false
    exposeTools: true
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata:
  name: second
spec:
  mcp:
    servers:
      exclude: [second]
",
        )
        .unwrap();

    let mcp = registry
        .resolve_agent("selected")
        .unwrap()
        .features
        .mcp
        .unwrap();
    assert_eq!(
        mcp.servers,
        McpServerSelection::Exclude(BTreeSet::from(["second".to_owned()]))
    );
    assert!(mcp.trace_propagation);
    assert!(!mcp.expose_tools);
}

#[test]
fn bundled_coding_definition_selects_all_mcp_servers_explicitly() {
    let mut registry = AgentDefinitionRegistry::new();
    for source in [
        DEFAULT_AGENT,
        CODING_AGENT,
        CODING_FEATURES,
        DEFAULT_PERMISSIONS,
        CODING_PERMISSIONS,
    ] {
        registry.load_yaml(source).unwrap();
    }

    let mcp = registry
        .resolve_agent("coding")
        .unwrap()
        .features
        .mcp
        .unwrap();
    assert_eq!(mcp.servers, McpServerSelection::Exclude(BTreeSet::new()));
    assert!(mcp.trace_propagation);
}

fn resolve_mcp_feature(source: &str) -> super::McpFeatureConfig {
    let agent = "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: selected\nspec:\n  features:\n    - featureSetRef: { name: selected }\n";
    let mut registry = AgentDefinitionRegistry::new();
    registry.load_yaml(source).unwrap();
    registry.load_yaml(agent).unwrap();
    registry
        .resolve_agent("selected")
        .unwrap()
        .features
        .mcp
        .unwrap()
}

#[test]
fn mcp_skills_feature_is_explicit_opt_in() {
    for (configuration, expected) in [
        ("{}", false),
        ("{ skills: false }", false),
        ("{ skills: true }", true),
    ] {
        let source = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: FeatureSet\nmetadata: {{ name: selected }}\nspec:\n  mcp: {configuration}\n"
        );
        let feature = resolve_mcp_feature(&source);
        assert_eq!(feature.skills, expected);
        assert!(!feature.expose_tools);
    }
}

#[test]
fn parses_multiple_documents_and_replaces_across_sources() {
    let mut registry = AgentDefinitionRegistry::new();
    registry
        .load_yaml(
            r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: shared
spec:
  systemPrompt: old
  features:
    - featureSetRef: { name: shared }
  permissions:
    - permissionSetRef: { name: shared }
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata:
  name: shared
spec:
  shell:
    root: $(session.workspace.root)
    systemPrompt: true
---
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: shared
spec:
  tools:
    - name: question
      permission: allow
",
        )
        .expect("multiple documents and cross-kind names should parse");
    registry
        .load_yaml(
            r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: shared
spec:
  systemPrompt: replacement
  features:
    - featureSetRef: { name: shared }
  permissions:
    - permissionSetRef: { name: shared }
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata:
  name: shared
spec:
  shell:
    root: $(session.workspace.root)
---
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: shared
spec:
  tools:
    - name: question
      permission: deny
",
        )
        .expect("later source should replace every matching resource kind");

    let resolved = registry.resolve_agent("shared").unwrap();
    assert_eq!(resolved.system_prompt.as_deref(), Some("replacement"));
    assert!(!resolved.features.shell.unwrap().system_prompt);
    assert_eq!(resolved.permissions.len(), 1);
    assert!(matches!(
        &resolved.permissions[0].permission,
        ResolvedPermission::Tool(effect) if effect.level == PermissionLevel::Deny
    ));
}

#[test]
fn duplicate_resources_reject_the_whole_source() {
    let mut registry = AgentDefinitionRegistry::new();
    registry
        .load_yaml(
            r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: stable
spec:
  systemPrompt: original
",
        )
        .unwrap();

    let error = registry
        .load_yaml(
            r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: stable
spec:
  systemPrompt: first replacement
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: stable
spec:
  systemPrompt: duplicate
",
        )
        .expect_err("same source must reject duplicate keys");

    assert!(matches!(
        error,
        AgentDefinitionError::DuplicateResource {
            document: 2,
            kind: AgentDefinitionResourceKind::Agent,
            ref name,
        } if name == "stable"
    ));
    assert_eq!(
        registry
            .resolve_agent("stable")
            .unwrap()
            .system_prompt
            .as_deref(),
        Some("original")
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn inheritance_composes_prompts_refs_and_whole_features() {
    let mut registry = AgentDefinitionRegistry::new();
    registry
        .load_yaml(
            r#"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: parent
spec:
  description: inherited description
  systemPrompt: "  parent prompt  "
  features:
    - featureSetRef: { name: parent }
  permissions:
    - permissionSetRef: { name: parent }
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: child
spec:
  inheritsFrom:
    name: parent
  systemPrompt: "  child prompt  "
  features:
    - featureSetRef: { name: child }
  permissions:
    - permissionSetRef: { name: child }
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata:
  name: parent
spec:
  filesystem:
    root: $(session.workspace.root)
    profile: auto
    systemPrompt: true
    defaultPermissions: true
  agents:
    root: $(session.workspace.root)
    filename: AGENTS.md
    systemPrompt: true
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata:
  name: child
spec:
  filesystem:
    root: $(session.workspace.root)
    profile: auto
  shell:
    root: $(session.workspace.root)
---
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: parent
spec:
  tools:
    - name: question
      permission: allow
---
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: child
spec:
  tools:
    - name: shell
      permission: deny
"#,
        )
        .unwrap();

    let resolved = registry.resolve_agent("child").unwrap();
    assert_eq!(
        resolved.description.as_deref(),
        Some("inherited description")
    );
    assert_eq!(
        resolved.system_prompt.as_deref(),
        Some("parent prompt\n\nchild prompt")
    );
    let filesystem = resolved.features.filesystem.unwrap();
    assert!(!filesystem.system_prompt);
    assert!(!filesystem.default_permissions);
    assert!(resolved.features.agents.is_some());
    assert!(resolved.features.shell.is_some());
    assert_eq!(resolved.permissions.len(), 2);
    assert!(matches!(
        &resolved.permissions[0].selector,
        AgentPermissionSelector::Name(name) if name == "question"
    ));
    assert!(matches!(
        &resolved.permissions[1].selector,
        AgentPermissionSelector::Name(name) if name == "shell"
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn ordered_inline_entries_compose_parent_then_child_left_to_right() {
    let mut registry = AgentDefinitionRegistry::new();
    registry
        .load_yaml(
            r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: parent }
spec:
  features:
    - filesystem: { root: $(session.workspace.root), profile: auto, systemPrompt: true }
  permissions:
    - { name: parent, permission: allow }
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: child }
spec:
  inheritsFrom: { name: parent }
  features:
    - featureSetRef: { name: base }
    - filesystem: { root: $(session.workspace.root), profile: auto }
    - shell: { root: $(session.workspace.root), systemPrompt: true }
  permissions:
    - permissionSetRef: { name: base }
    - { feature: shell, permission: deny }
    - permissionSetRef: { name: final }
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: { name: base }
spec:
  filesystem: { root: $(session.workspace.root), profile: auto, defaultPermissions: true }
  skills: { root: $(session.workspace.root) }
---
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata: { name: base }
spec:
  tools:
    - { name: first, permission: ask }
    - { name: second, permission: allow }
---
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata: { name: final }
spec:
  tools:
    - { name: last, permission: deny }
",
        )
        .unwrap();

    let resolved = registry.resolve_agent("child").unwrap();
    let filesystem = resolved.features.filesystem.unwrap();
    assert!(!filesystem.system_prompt);
    assert!(!filesystem.default_permissions);
    assert!(resolved.features.skills.is_some());
    assert!(resolved.features.shell.unwrap().system_prompt);
    let selectors = resolved
        .permissions
        .iter()
        .map(|entry| match &entry.selector {
            AgentPermissionSelector::Name(name) => name.as_str(),
            AgentPermissionSelector::Feature(AgentFeature::Shell) => "feature:shell",
            selector @ AgentPermissionSelector::Feature(_) => {
                panic!("unexpected selector: {selector:?}")
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        selectors,
        ["parent", "first", "second", "feature:shell", "last"]
    );
}

#[test]
fn empty_agent_feature_and_permission_lists_are_valid() {
    let mut registry = AgentDefinitionRegistry::new();
    registry
        .load_yaml(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: empty }\nspec: { features: [], permissions: [] }\n",
        )
        .unwrap();

    let resolved = registry.resolve_agent("empty").unwrap();
    assert_eq!(resolved.features, super::ResolvedAgentFeatures::default());
    assert!(resolved.permissions.is_empty());
}

#[test]
fn nested_input_permissions_parse_in_order() {
    let mut registry = AgentDefinitionRegistry::new();
    registry
        .load_yaml(
            r#"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: inputs }
spec:
  permissions:
    - name: mcp_sessions
      permission:
        inputs:
          - match:
              - pointer: /action
                value: { wildcard: "sessions.*" }
              - pointer: /options/dryRun
                value: { exact: false }
            effect: deny
"#,
        )
        .unwrap();

    let resolved = registry.resolve_agent("inputs").unwrap();
    let ResolvedPermission::Inputs(rules) = &resolved.permissions[0].permission else {
        panic!("expected input permission");
    };
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].conditions.len(), 2);
    assert_eq!(rules[0].conditions[0].pointer.as_str(), "/action");
    assert_eq!(rules[0].effect.level, PermissionLevel::Deny);
}

#[test]
fn nested_input_exact_numbers_are_canonical_without_float_loss() {
    let mut registry = AgentDefinitionRegistry::new();
    registry
        .load_yaml(
            r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: numbers }
spec:
  permissions:
    - name: tool
      permission:
        inputs:
          - match:
              - pointer: /value
                value: { exact: 0.0000000000000000001 }
            effect: allow
",
        )
        .unwrap();
    let resolved = registry.resolve_agent("numbers").unwrap();
    let ResolvedPermission::Inputs(rules) = &resolved.permissions[0].permission else {
        panic!("expected input permission");
    };
    assert!(matches!(
        &rules[0].conditions[0].value,
        crate::PermissionInputValueMatcher::Exact(crate::PermissionInputScalar::Number(value))
            if value == "1e-19"
    ));
}

#[test]
fn nested_input_permissions_reject_malformed_contract_shapes() {
    for permission in [
        "inputs: [{ match: [], effect: deny }]",
        "inputs: [{ match: [{ pointer: action, value: { exact: x } }], effect: deny }]",
        "inputs: [{ match: [{ pointer: /action, value: { wildcard: 'a*b*' } }], effect: deny }]",
        "inputs: [{ match: [{ pointer: /action, value: { exact: {}, wildcard: 'a*' } }], effect: deny }]",
        "inputs: [{ match: [{ pointer: /action, value: { exact: [a] } }], effect: deny }]",
    ] {
        let source = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: invalid }}\nspec:\n  permissions:\n    - name: tool\n      permission:\n        {}\n",
            permission.replace('\n', "\n        ")
        );
        assert!(
            AgentDefinitionRegistry::new().load_yaml(&source).is_err(),
            "{permission}"
        );
    }
}

#[test]
fn legacy_agent_reference_fields_are_rejected_without_aliases() {
    for field in ["featureSetRefs", "permissionSetRefs"] {
        let source = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: invalid }}\nspec:\n  {field}: []\n"
        );
        assert!(matches!(
            AgentDefinitionRegistry::new().load_yaml(&source),
            Err(AgentDefinitionError::Yaml { .. })
        ));
    }
}

#[test]
fn inline_entries_reject_null_empty_multiple_unknown_and_malformed_shapes() {
    let invalid_specs = [
        "features: null",
        "features: [null]",
        "features: [{}]",
        "features: [{ filesystem: { root: $(session.workspace.root), profile: auto }, shell: { root: $(session.workspace.root) } }]",
        "features: [{ browser: {} }]",
        "features: [{ featureSetRef: null }]",
        "features: [{ featureSetRef: {} }]",
        "features: [{ filesystem: null }]",
        "features: [{ filesystem: {} }]",
        "permissions: null",
        "permissions: [null]",
        "permissions: [{}]",
        "permissions: [{ permissionSetRef: { name: set }, name: tool, permission: allow }]",
        "permissions: [{ unknown: true }]",
        "permissions: [{ permissionSetRef: null }]",
        "permissions: [{ permissionSetRef: {} }]",
        "permissions: [{ name: tool }]",
        "permissions: [{ name: tool, feature: shell, permission: allow }]",
        "permissions: [{ name: tool, permission: null }]",
        "permissions: [{ name: tool, permission: { resources: {} } }]",
    ];

    for spec in invalid_specs {
        let source = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: invalid }}\nspec: {{ {spec} }}\n"
        );
        assert!(
            AgentDefinitionRegistry::new().load_yaml(&source).is_err(),
            "source should fail strict parsing:\n{source}"
        );
    }
}

#[test]
fn inline_semantic_errors_report_indexed_paths() {
    let invalid = [
        (
            "features:\n    - shell: { root: /tmp }",
            "spec.features[0].shell.root",
        ),
        (
            "features:\n    - mcp: { servers: {} }",
            "spec.features[0].mcp.servers",
        ),
        (
            "permissions:\n    - { name: '', permission: allow }",
            "spec.permissions[0].name",
        ),
        (
            "permissions:\n    - name: shell\n      permission: { resources: {} }",
            "spec.permissions[0].permission.resources",
        ),
    ];

    for (spec, expected_field) in invalid {
        let source = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: invalid }}\nspec:\n  {spec}\n"
        );
        assert!(matches!(
            AgentDefinitionRegistry::new().load_yaml(&source),
            Err(AgentDefinitionError::InvalidValue { field, .. }) if field == expected_field
        ));
    }
}

#[test]
fn inline_shape_errors_report_indexed_paths() {
    for (spec, expected_path) in [
        ("features: [null]", "spec.features[0]"),
        ("features: [{ filesystem: {} }]", "spec.features[0]"),
        ("permissions: [null]", "spec.permissions[0]"),
        (
            "permissions: [{ permissionSetRef: {} }]",
            "spec.permissions[0]",
        ),
    ] {
        let source = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: invalid }}\nspec: {{ {spec} }}\n"
        );
        let error = AgentDefinitionRegistry::new()
            .load_yaml(&source)
            .unwrap_err();
        assert!(
            error.to_string().contains(expected_path),
            "error should contain `{expected_path}`: {error}"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn missing_references_and_inheritance_cycles_fail() {
    let mut registry = AgentDefinitionRegistry::new();
    registry
        .load_yaml(
            r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: missing-parent
spec:
  inheritsFrom:
    name: absent
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: missing-feature
spec:
  features:
    - featureSetRef: { name: absent }
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: missing-permission
spec:
  permissions:
    - permissionSetRef: { name: absent }
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: a
spec:
  inheritsFrom:
    name: b
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: b
spec:
  inheritsFrom:
    name: c
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: c
spec:
  inheritsFrom:
    name: a
",
        )
        .unwrap();

    for (agent, kind) in [
        ("missing-parent", AgentDefinitionResourceKind::Agent),
        ("missing-feature", AgentDefinitionResourceKind::FeatureSet),
        (
            "missing-permission",
            AgentDefinitionResourceKind::PermissionSet,
        ),
    ] {
        assert!(matches!(
            registry.resolve_agent(agent),
            Err(AgentDefinitionError::MissingReference {
                kind: actual_kind,
                ref name,
                ..
            }) if actual_kind == kind && name == "absent"
        ));
    }
    assert!(matches!(
        registry.resolve_agent("not-registered"),
        Err(AgentDefinitionError::MissingReference {
            kind: AgentDefinitionResourceKind::Agent,
            ref name,
            field: None,
        }) if name == "not-registered"
    ));
    assert!(matches!(
        registry.resolve_agent("a"),
        Err(AgentDefinitionError::InheritanceCycle { agents })
            if agents == ["a", "b", "c", "a"]
    ));

    assert!(matches!(
        registry.resolve_agent("missing-feature"),
        Err(AgentDefinitionError::MissingReference {
            field: Some(field),
            ..
        }) if field == "spec.features[0].featureSetRef.name"
    ));
    assert!(matches!(
        registry.resolve_agent("missing-permission"),
        Err(AgentDefinitionError::MissingReference {
            field: Some(field),
            ..
        }) if field == "spec.permissions[0].permissionSetRef.name"
    ));
}

#[test]
fn resolving_all_agents_validates_unselected_definitions() {
    let mut registry = AgentDefinitionRegistry::new();
    registry
        .load_yaml(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: valid\nspec: {}\n---\napiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: invalid\nspec:\n  features:\n    - featureSetRef: { name: missing }\n",
        )
        .unwrap();

    assert!(matches!(
        registry.resolve_agents(),
        Err(AgentDefinitionError::MissingReference {
            kind: AgentDefinitionResourceKind::FeatureSet,
            ref name,
            ..
        }) if name == "missing"
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn rejects_unknown_fields_features_values_and_permission_inputs() {
    let invalid_sources = [
        r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: invalid
unknown: true
spec: {}
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: invalid
spec:
  model: claude
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata:
  name: invalid
spec:
  filesystem: null
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata:
  name: invalid
spec:
  browser: {}
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata:
  name: invalid
spec:
  filesystem:
    root: $(session.workspace.root)
    profile: full
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata:
  name: invalid
spec:
  mcp:
    serversFrom: session
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: invalid
spec:
  tools:
    - name: null
      feature: shell
      permission: allow
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: invalid
spec:
  tools:
    - name: waltr
      permission:
        inputs:
          - match:
              action: sessions.*
            effect: deny
",
    ];

    for source in invalid_sources {
        assert!(
            matches!(
                AgentDefinitionRegistry::new().load_yaml(source),
                Err(AgentDefinitionError::Yaml { .. })
            ),
            "source should fail strict deserialization:\n{source}"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn rejects_unsupported_literals_and_invalid_permission_shapes() {
    let invalid_sources = [
        r"
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata:
  name: invalid
spec:
  filesystem:
    root: /tmp
    profile: auto
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata:
  name: invalid
spec:
  agents:
    root: $(session.workspace.root)
    filename: INSTRUCTIONS.md
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: invalid
spec:
  tools:
    - name: shell
      feature: shell
      permission: allow
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: invalid
spec:
  tools:
    - permission: allow
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: invalid
spec:
  tools:
    - name: shell
      permission:
        resources: {}
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: invalid
spec:
  tools:
    - name: shell
      permission:
        resources:
          commands:
            - match:
                names: []
              effect: ask
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: invalid
spec:
  tools:
    - feature: browser
      permission: allow
",
        r"
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata:
  name: invalid
spec:
  tools:
    - feature: agents
      permission: allow
",
    ];

    for source in invalid_sources {
        assert!(
            AgentDefinitionRegistry::new().load_yaml(source).is_err(),
            "source should fail validation:\n{source}"
        );
    }
}

#[test]
fn rejects_unsupported_api_version() {
    let error = AgentDefinitionRegistry::new()
        .load_yaml(
            r"
apiVersion: waltr.dev/v1
kind: Agent
metadata:
  name: invalid
spec: {}
",
        )
        .unwrap_err();

    assert!(matches!(
        error,
        AgentDefinitionError::UnsupportedApiVersion {
            document: 1,
            ref api_version,
        } if api_version == "waltr.dev/v1"
    ));
}
