//! Sandbox-backed skill catalog and loading.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    AgentMiddleware, HookOutcome,
    context::{
        MiddlewareStateAccess, RequestContext, SystemPromptParts, TemplateRenderContext,
        TemplateSearchContext, ToolCallContext, ToolResultContext,
    },
};
use crate::{
    Result, ToolResult,
    resource::{
        SessionTemplateKind, SessionTemplateOption, SessionTemplateSearchTarget,
        TemplateSearchResults, prompt_score,
    },
    sandbox_gateway::SandboxHandle,
    template::{TemplateReference, TemplateReferenceRender, template_error},
    tools::skills::{SkillDefinition, definition::parse_skill_frontmatter, render_skills_catalog},
};

const STATE: &str = "skills";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SkillState {
    #[serde(default)]
    catalog: Vec<SkillDefinition>,
    #[serde(default)]
    revealed_roots: Vec<PathBuf>,
    #[serde(default)]
    loaded: Vec<LoadedSkill>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LoadedSkill {
    name: String,
    path: PathBuf,
    content: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SandboxSkillsMiddleware {
    root: PathBuf,
    reveal_roots: Vec<PathBuf>,
    sandbox: SandboxHandle,
}

impl SandboxSkillsMiddleware {
    pub(crate) fn new(root: PathBuf, reveal_roots: Vec<PathBuf>, sandbox: SandboxHandle) -> Self {
        Self {
            root,
            reveal_roots,
            sandbox,
        }
    }

    async fn discover(&self) -> Result<Vec<SkillDefinition>> {
        let root = PathBuf::from(
            self.sandbox
                .resolve(&self.root.to_string_lossy())
                .await?
                .canonical_path,
        );
        let paths = self
            .sandbox
            .glob(&root.to_string_lossy(), ".agents/skills/*/SKILL.md")
            .await?
            .entries;
        self.definitions(paths).await
    }

    async fn discover_under(&self, path: &Path) -> Result<Vec<SkillDefinition>> {
        let paths = self
            .sandbox
            .glob(&path.to_string_lossy(), "**/.agents/skills/*/SKILL.md")
            .await?
            .entries;
        self.definitions(paths).await
    }

    async fn definitions(&self, paths: Vec<String>) -> Result<Vec<SkillDefinition>> {
        let mut skills = Vec::new();
        for path in paths {
            let skill_path = PathBuf::from(path);
            let Some(skill_dir) = skill_path.parent() else {
                continue;
            };
            let Some(skills_dir) = skill_dir.parent() else {
                continue;
            };
            let Some(root) = skills_dir.parent().and_then(Path::parent) else {
                continue;
            };
            let text = self
                .sandbox
                .read_file(&skill_path.to_string_lossy())
                .await?
                .content;
            if let Some(metadata) = parse_skill_frontmatter(&text) {
                skills.push(SkillDefinition {
                    name: metadata.name,
                    description: metadata.description,
                    root: root.to_path_buf(),
                    skills_dir: skills_dir.to_path_buf(),
                    skill_dir: skill_dir.to_path_buf(),
                    skill_path,
                });
            }
        }
        Ok(skills)
    }

    async fn loaded_content(&self, skill: &SkillDefinition) -> Result<String> {
        let text = self
            .sandbox
            .read_file(&skill.skill_path.to_string_lossy())
            .await?
            .content;
        let mut resources = Vec::new();
        for directory in ["scripts", "references", "assets"] {
            let base = skill.skill_dir.join(directory);
            let entries = self.sandbox.glob(&base.to_string_lossy(), "**/*").await;
            if let Ok(entries) = entries {
                for path in entries.entries {
                    if let Ok(relative) = Path::new(&path).strip_prefix(&skill.skill_dir) {
                        resources.push(relative.to_string_lossy().into_owned());
                    }
                }
            }
        }
        resources.sort();
        let resources = resources
            .into_iter()
            .map(|path| format!("\n- {path}"))
            .collect::<String>();
        Ok(format!(
            "<skill_content name=\"{}\">\nSkill directory: {}\n\n{}\n\n<skill_resources>{resources}\n</skill_resources>\n</skill_content>",
            skill.name,
            skill.skill_dir.display(),
            text.trim()
        ))
    }
}

fn select_skill<'a>(
    catalog: &'a [SkillDefinition],
    name: &str,
    root: Option<&str>,
) -> Option<&'a SkillDefinition> {
    let mut matches = catalog.iter().filter(|skill| {
        skill.name == name && root.is_none_or(|root| skill.root == Path::new(root))
    });
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

impl SandboxSkillsMiddleware {
    async fn build_catalog(
        &self,
        ctx: &mut RequestContext<'_>,
        prompt: &mut SystemPromptParts,
    ) -> Result<HookOutcome> {
        let state = if let Some(state) = ctx.state_as::<SkillState>(STATE)? {
            state
        } else {
            let state = SkillState {
                catalog: self.discover().await?,
                revealed_roots: vec![
                    PathBuf::from(
                        self.sandbox
                            .resolve(&self.root.to_string_lossy())
                            .await?
                            .canonical_path,
                    )
                    .join(".agents/skills"),
                ],
                loaded: Vec::new(),
            };
            ctx.set_state_as(STATE, &state)?;
            state
        };
        if let Some(text) = render_skills_catalog(&state.catalog, "Available skills:") {
            prompt.push(text);
        }
        Ok(HookOutcome::Continue)
    }
}

impl SandboxSkillsMiddleware {
    pub(crate) async fn load(
        &self,
        ctx: &mut ToolCallContext<'_>,
        name: &str,
        root: Option<&str>,
    ) -> Result<HookOutcome> {
        let mut state = ctx.state_as::<SkillState>(STATE)?.unwrap_or_default();
        let skill =
            select_skill(&state.catalog, name, root).ok_or_else(|| crate::LlmError::Protocol {
                provider: "skills",
                message: "skill is not visible".into(),
            })?;
        let content = if state
            .loaded
            .iter()
            .any(|loaded| loaded.path == skill.skill_path)
        {
            format!(
                "Skill '{}' from root {} is already loaded.",
                skill.name,
                skill.root.display()
            )
        } else {
            let content = self.loaded_content(skill).await?;
            state.loaded.push(LoadedSkill {
                name: skill.name.clone(),
                path: skill.skill_path.clone(),
                content: content.clone(),
            });
            ctx.set_state_as(STATE, &state)?;
            content
        };
        Ok(HookOutcome::ReplaceToolResult(ToolResult::new(
            ctx.call.id.clone(),
            ctx.call.name.clone(),
            content,
        )))
    }
}

impl AgentMiddleware for SandboxSkillsMiddleware {
    async fn build_system_prompt(
        &self,
        ctx: &mut RequestContext<'_>,
        prompt: &mut SystemPromptParts,
    ) -> Result<HookOutcome> {
        self.build_catalog(ctx, prompt).await
    }
    async fn after_tool_call(&self, ctx: &mut ToolResultContext<'_>) -> Result<HookOutcome> {
        if self.reveal_roots.is_empty() {
            return Ok(HookOutcome::Continue);
        }
        let Some(call) = ctx.call else {
            return Ok(HookOutcome::Continue);
        };
        if ctx.result.is_error {
            return Ok(HookOutcome::Continue);
        }
        let (path, walk_children) = match call.name.as_str() {
            "read" => (
                call.arguments
                    .get("filePath")
                    .and_then(serde_json::Value::as_str),
                false,
            ),
            "glob" | "grep" => (
                call.arguments
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .or(Some(".")),
                true,
            ),
            _ => return Ok(HookOutcome::Continue),
        };
        let Some(path) = path else {
            return Ok(HookOutcome::Continue);
        };
        let target = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.root.join(path)
        };
        let target = match self.sandbox.resolve(&target.to_string_lossy()).await {
            Ok(data)
                if (walk_children && data.path_kind == "directory")
                    || (!walk_children && data.path_kind == "file") =>
            {
                PathBuf::from(data.canonical_path)
            }
            Err(_) => return Ok(HookOutcome::Continue),
            Ok(_) => return Ok(HookOutcome::Continue),
        };
        let root = PathBuf::from(
            self.sandbox
                .resolve(&self.root.to_string_lossy())
                .await?
                .canonical_path,
        );
        let mut visible = false;
        for reveal_root in &self.reveal_roots {
            if let Ok(data) = self.sandbox.resolve(&reveal_root.to_string_lossy()).await {
                visible |= target.starts_with(Path::new(&data.canonical_path));
            }
        }
        if !visible {
            return Ok(HookOutcome::Continue);
        }
        let mut state = ctx.state_as::<SkillState>(STATE)?.unwrap_or_default();
        let initial_state = state.clone();
        let mut reminders = Vec::new();
        let mut roots = Vec::new();
        let start = if walk_children {
            target.as_path()
        } else {
            target.parent().unwrap_or(&target)
        };
        for ancestor in start.ancestors() {
            if !ancestor.starts_with(&root) {
                break;
            }
            roots.push(ancestor.to_path_buf());
            if ancestor == root {
                break;
            }
        }
        for root in roots {
            let skills_dir = root.join(".agents/skills");
            if state.revealed_roots.contains(&skills_dir) && !walk_children {
                continue;
            }
            let candidates = self.discover_under(&root).await?;
            if !state.revealed_roots.contains(&skills_dir) {
                state.revealed_roots.push(skills_dir);
            }
            let mut added = Vec::new();
            for skill in candidates {
                if !walk_children && skill.root != root {
                    continue;
                }
                if !state
                    .catalog
                    .iter()
                    .any(|known| known.skill_path == skill.skill_path)
                {
                    added.push(skill.clone());
                    state.catalog.push(skill);
                }
            }
            if let Some(reminder) = crate::tools::skills::render_skills_reminder(&added, &root) {
                reminders.push(reminder);
            }
        }
        if !reminders.is_empty() {
            ctx.result.content.push_str("\n\n<system-reminder>\n");
            ctx.result.content.push_str(&reminders.join("\n\n"));
            ctx.result.content.push_str("\n</system-reminder>");
        }
        if state != initial_state {
            ctx.set_state_as(STATE, &state)?;
        }
        Ok(HookOutcome::Continue)
    }

    async fn search_templates(
        &self,
        ctx: &TemplateSearchContext<'_>,
        results: &mut TemplateSearchResults,
    ) -> Result<()> {
        if ctx.request.target != SessionTemplateSearchTarget::Prompts
            || !ctx.request.includes(SessionTemplateKind::Prompt)
        {
            return Ok(());
        }
        let catalog = match ctx.state_as::<SkillState>(STATE)? {
            Some(state) => state.catalog,
            None => self.discover().await?,
        };
        let query = ctx
            .request
            .query
            .trim()
            .trim_start_matches('/')
            .trim_start_matches("skill:")
            .to_lowercase();
        for skill in catalog {
            if let Some(score) = prompt_score(&skill.name, &skill.description, &query) {
                results.push(score, SessionTemplateOption { kind: SessionTemplateKind::Prompt, label: format!("skill:{}", skill.name), detail: skill.description.clone(), template: format!("/skill:{}", skill.name), metadata: json!({"promptType":"skill","name":skill.name,"description":skill.description,"root":skill.root.display().to_string()}) });
            }
        }
        Ok(())
    }

    async fn render_template_reference(
        &self,
        ctx: &mut TemplateRenderContext<'_>,
        reference: &TemplateReference,
    ) -> Result<TemplateReferenceRender> {
        let TemplateReference::Prompt {
            namespace, name, ..
        } = reference
        else {
            return Ok(TemplateReferenceRender::Continue);
        };
        if namespace != "skill" {
            return Ok(TemplateReferenceRender::Continue);
        }
        let catalog = match ctx.state_as::<SkillState>(STATE)? {
            Some(state) => state.catalog,
            None => self.discover().await?,
        };
        let Some(skill) = select_skill(&catalog, name, None) else {
            return Ok(TemplateReferenceRender::Rendered(template_error(
                "skill",
                "skill is not visible",
            )));
        };
        match self.loaded_content(skill).await {
            Ok(content) => Ok(TemplateReferenceRender::TemplateResult {
                kind: "skill".into(),
                tool: Some("load".into()),
                source: Some(name.clone()),
                content,
                render_nested: false,
            }),
            Err(error) => Ok(TemplateReferenceRender::Rendered(template_error(
                "skill", error,
            ))),
        }
    }
}
