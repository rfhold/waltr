//! Filesystem tool configuration.

mod config;
mod formatting;
pub mod lsp;
mod ops;
mod patch;
mod path;
mod permissions;
mod profile;
mod schema;

use serde::de::DeserializeOwned;

pub use config::FilesystemConfig;
pub use profile::{FilesystemToolProfile, FilesystemToolSet, ResolvedFilesystemToolProfile};
pub use schema::{
    ApplyPatchInput, DeleteInput, EditInput, FilesystemToolKind, GlobInput, GrepInput, GrepMatch,
    ReadInput, WriteInput,
};

use formatting::truncate_line;
use patch::{patch_path, skip_patch_body};
use path::{filesystem_error, include_relative_path};
pub(crate) use path::{model_path, resolve_existing_path};
use schema::GrepSearch;

use crate::{
    AgentTool, ModelRef, Result, ToolCall, ToolExecutionContext, ToolResult,
    middleware::tool_metadata::RootHintsMiddleware,
};

impl FilesystemConfig {
    /// Convert exposed filesystem capabilities into generic agent tools.
    #[must_use]
    pub fn into_agent_tools(self, model: &ModelRef) -> Vec<AgentTool> {
        let tools = self.exposed_tools(model).all();
        tools
            .into_iter()
            .map(|kind| self.clone().into_agent_tool(kind))
            .collect()
    }

    /// Build the per-request root-hint metadata middleware for the exposed tools.
    pub(crate) fn root_hints_middleware(&self, model: &ModelRef) -> RootHintsMiddleware {
        let root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        let tool_names = self
            .exposed_tools(model)
            .all()
            .into_iter()
            .map(|kind| kind.name().to_owned())
            .collect();
        RootHintsMiddleware::new(
            tool_names,
            format!(
                " Paths must be relative to the filesystem root or absolute paths inside the filesystem root. Filesystem root: {}. Use path '.' or omit optional path fields to search the root. Never use '/', a parent directory, or any path outside this root.",
                root.display()
            ),
        )
    }

    fn into_agent_tool(self, kind: FilesystemToolKind) -> AgentTool {
        AgentTool::new(kind.definition(), move |context| {
            self.call_agent_tool(kind, &context)
        })
    }

    fn call_agent_tool(
        &self,
        kind: FilesystemToolKind,
        context: &ToolExecutionContext,
    ) -> Result<ToolResult> {
        let output = match kind {
            FilesystemToolKind::Read => self.read_unscoped(&tool_input(context.call())?)?,
            FilesystemToolKind::Glob => self.glob_unscoped(&tool_input(context.call())?)?,
            FilesystemToolKind::Grep => self.grep_unscoped(&tool_input(context.call())?)?,
            FilesystemToolKind::Edit => self.edit_unscoped(&tool_input(context.call())?)?,
            FilesystemToolKind::Write => self.write_unscoped(&tool_input(context.call())?)?,
            FilesystemToolKind::Delete => self.delete_unscoped(&tool_input(context.call())?)?,
            FilesystemToolKind::ApplyPatch => {
                self.apply_patch_unscoped(&tool_input(context.call())?)?
            }
        };
        Ok(context.result(output))
    }
}

fn tool_input<T>(call: &ToolCall) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(call.arguments.clone()).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{ModelApi, ModelRef, ProviderId};

    use super::{
        ApplyPatchInput, DeleteInput, EditInput, FilesystemConfig, FilesystemToolKind,
        FilesystemToolProfile, GlobInput, GrepInput, ReadInput, ResolvedFilesystemToolProfile,
        WriteInput,
    };

    #[test]
    fn auto_profile_uses_structured_patch_for_newer_gpt_models() {
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5-nano", ModelApi::OpenAiResponses);
        let config = FilesystemConfig::new(".");

        assert_eq!(
            config.resolve_profile(&model),
            ResolvedFilesystemToolProfile::StructuredPatch
        );
        assert_eq!(
            config.exposed_tools(&model).edit_tools,
            vec![
                FilesystemToolKind::Edit,
                FilesystemToolKind::Write,
                FilesystemToolKind::Delete,
                FilesystemToolKind::ApplyPatch,
            ]
        );
    }

    #[test]
    fn auto_profile_uses_exact_edit_for_gpt_4_and_anthropic() {
        let gpt4 = ModelRef::new(ProviderId::OpenAi, "gpt-4.1", ModelApi::OpenAiResponses);
        let claude = ModelRef::new(
            ProviderId::Anthropic,
            "claude-haiku-4-5",
            ModelApi::AnthropicMessages,
        );
        let config = FilesystemConfig::new(".");

        assert_eq!(
            config.resolve_profile(&gpt4),
            ResolvedFilesystemToolProfile::ExactEdit
        );
        assert_eq!(
            config.resolve_profile(&claude),
            ResolvedFilesystemToolProfile::ExactEdit
        );
    }

    #[test]
    fn explicit_profile_overrides_auto_detection() {
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5-nano", ModelApi::OpenAiResponses);
        let config = FilesystemConfig::new(".").with_profile(FilesystemToolProfile::WholeFileWrite);

        assert_eq!(
            config.resolve_profile(&model),
            ResolvedFilesystemToolProfile::WholeFileWrite
        );
    }

    #[test]
    fn read_returns_opencode_file_output() {
        let temp = TempRoot::new();
        temp.write("notes.txt", "hello\nfrom waltr\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .read(&ReadInput::new("notes.txt"))
                .expect("read file"),
            format!(
                "<path>{}</path>\n<type>file</type>\n<content>\n1: hello\n2: from waltr\n\n(End of file - total 2 lines)\n</content>",
                temp.path().join("notes.txt").display()
            )
        );
    }

    #[test]
    fn read_accepts_absolute_paths_inside_root() {
        let temp = TempRoot::new();
        temp.write("notes.txt", "hello\n");
        let config = FilesystemConfig::new(temp.path());

        assert!(
            config
                .read(&ReadInput::new(
                    temp.path().join("notes.txt").display().to_string()
                ))
                .expect("read absolute path")
                .contains("1: hello")
        );
    }

    #[test]
    fn read_accepts_pasted_path_tags() {
        let temp = TempRoot::new();
        temp.write("notes.txt", "hello\n");
        let config = FilesystemConfig::new(temp.path());
        let file_path = format!("<path>{}</path>", temp.path().join("notes.txt").display());

        assert!(
            config
                .read(&ReadInput::new(file_path))
                .expect("read pasted path tag")
                .contains("1: hello")
        );
    }

    #[test]
    fn root_escape_error_reports_rejected_path_and_root() {
        let temp = TempRoot::new();
        let config = FilesystemConfig::new(temp.path());
        let error = config
            .glob(&GlobInput::new("**/*").with_path("/"))
            .expect_err("root path should escape configured root")
            .to_string();

        assert!(error.contains("path escapes filesystem root"));
        assert!(error.contains('/'));
        assert!(error.contains(&temp.path().display().to_string()));
        assert!(error.contains("Use a path relative to the filesystem root"));
    }

    #[test]
    fn read_returns_opencode_directory_output() {
        let temp = TempRoot::new();
        temp.write("src/lib.rs", "");
        temp.write("README.md", "");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config.read(&ReadInput::new(".")).expect("read directory"),
            format!(
                "<path>{}</path>\n<type>directory</type>\n<entries>\nREADME.md\nsrc/\n(2 entries)\n</entries>",
                temp.path().display()
            )
        );
    }

    #[test]
    fn glob_returns_opencode_absolute_path_output() {
        let temp = TempRoot::new();
        temp.write("src/lib.rs", "");
        temp.write("src/main.rs", "");
        temp.write("README.md", "");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config.glob(&GlobInput::new("**/*.rs")).expect("glob files"),
            format!(
                "{}\n{}",
                temp.path().join("src/lib.rs").display(),
                temp.path().join("src/main.rs").display()
            )
        );
    }

    #[test]
    fn grep_returns_opencode_match_output() {
        let temp = TempRoot::new();
        temp.write("a.txt", "alpha\nbeta\n");
        temp.write("nested/b.txt", "gamma\nalpha beta\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config.grep(&GrepInput::new("alpha")).expect("grep files"),
            format!(
                "Found 2 matches\n{}:\n  Line 1: alpha\n{}:\n  Line 2: alpha beta",
                temp.path().join("a.txt").display(),
                temp.path().join("nested/b.txt").display()
            )
        );
    }

    #[test]
    fn grep_supports_file_path_and_include() {
        let temp = TempRoot::new();
        temp.write("a.txt", "alpha\n");
        temp.write("a.md", "alpha\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .grep(
                    &GrepInput::new("alpha")
                        .with_path("a.txt")
                        .with_include("*.txt")
                )
                .expect("grep file"),
            format!(
                "Found 1 matches\n{}:\n  Line 1: alpha",
                temp.path().join("a.txt").display()
            )
        );
    }

    #[test]
    fn edit_replaces_exactly_one_match() {
        let temp = TempRoot::new();
        temp.write("notes.txt", "before\ntarget\nafter\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .edit(&EditInput::new("notes.txt", "target", "replacement"))
                .expect("edit file"),
            "Edit applied successfully."
        );

        assert_eq!(temp.read("notes.txt"), "before\nreplacement\nafter\n");
    }

    #[test]
    fn write_creates_parent_directories_and_file() {
        let temp = TempRoot::new();
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .write(&WriteInput::new("created by write\n", "nested/new.txt"))
                .expect("write file"),
            "Wrote file successfully."
        );

        assert_eq!(temp.read("nested/new.txt"), "created by write\n");
    }

    #[test]
    fn delete_removes_files() {
        let temp = TempRoot::new();
        temp.write("deleted.txt", "remove me\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .delete(&DeleteInput::new(
                    temp.path().join("deleted.txt").display().to_string(),
                ))
                .expect("delete file"),
            "Deleted file successfully."
        );

        assert!(!temp.path().join("deleted.txt").exists());
    }

    #[test]
    fn apply_patch_adds_files() {
        let temp = TempRoot::new();
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .apply_patch(&ApplyPatchInput::new(
                    "*** Begin Patch\n*** Add File: added.txt\n+hello\n+from patch\n*** End Patch",
                ))
                .expect("apply add patch"),
            "Success. Updated the following files:\nA added.txt"
        );

        assert_eq!(temp.read("added.txt"), "hello\nfrom patch\n");
    }

    #[test]
    fn apply_patch_updates_files() {
        let temp = TempRoot::new();
        temp.write("updated.txt", "old\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .apply_patch(&ApplyPatchInput::new(
                    "*** Begin Patch\n*** Update File: updated.txt\n@@\n-old\n+new\n*** End Patch",
                ))
                .expect("apply update patch"),
            "Success. Updated the following files:\nM updated.txt"
        );

        assert_eq!(temp.read("updated.txt"), "new\n");
    }

    #[test]
    fn apply_patch_deletes_files() {
        let temp = TempRoot::new();
        temp.write("deleted.txt", "remove me\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .apply_patch(&ApplyPatchInput::new(
                    "*** Begin Patch\n*** Delete File: deleted.txt\n*** End Patch",
                ))
                .expect("apply delete patch"),
            "Success. Updated the following files:\nD deleted.txt"
        );

        assert!(!temp.path().join("deleted.txt").exists());
    }

    #[test]
    fn apply_patch_supports_multiple_operations() {
        let temp = TempRoot::new();
        temp.write("updated.txt", "old\n");
        temp.write("deleted.txt", "remove me\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .apply_patch(&ApplyPatchInput::new(
                    "*** Begin Patch\n*** Update File: updated.txt\n@@\n-old\n+new\n*** Delete File: deleted.txt\n*** Add File: added.txt\n+created\n*** End Patch",
                ))
                .expect("apply multi patch"),
            "Success. Updated the following files:\nM updated.txt\nD deleted.txt\nA added.txt"
        );

        assert_eq!(temp.read("updated.txt"), "new\n");
        assert!(!temp.path().join("deleted.txt").exists());
        assert_eq!(temp.read("added.txt"), "created\n");
    }

    #[test]
    fn apply_patch_accepts_path_on_next_line() {
        let temp = TempRoot::new();
        temp.write("updated.txt", "old\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .apply_patch(&ApplyPatchInput::new(
                    "*** Begin Patch\n*** Update File\nupdated.txt\n@@\n-old\n+new\n*** End Patch",
                ))
                .expect("apply patch with path on next line"),
            "Success. Updated the following files:\nM updated.txt"
        );

        assert_eq!(temp.read("updated.txt"), "new\n");
    }

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "waltr-agent-filesystem-test-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temp root");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn write(&self, path: &str, content: &str) {
            let path = self.path.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent directory");
            }
            fs::write(path, content).expect("write temp file");
        }

        fn read(&self, path: &str) -> String {
            fs::read_to_string(self.path.join(path)).expect("read temp file")
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.path));
        }
    }
}
