//! Filesystem permission claim construction.

use std::path::Path;

use crate::{PermissionClaim, PermissionOperation, PermissionResource, Result, ToolCall};

use super::{
    ApplyPatchInput, DeleteInput, EditInput, FilesystemConfig, FilesystemToolKind, GlobInput,
    GrepInput, ReadInput, WriteInput, filesystem_error, model_path, patch_path, skip_patch_body,
    tool_input,
};

impl FilesystemConfig {
    pub(crate) fn permission_claims(
        &self,
        kind: FilesystemToolKind,
        call: &ToolCall,
    ) -> Result<Vec<PermissionClaim>> {
        match kind {
            FilesystemToolKind::Read => {
                let input = tool_input::<ReadInput>(call)?;
                Ok(vec![self.path_claim(
                    kind.permission_operation(),
                    &model_path(&input.file_path),
                    true,
                )?])
            }
            FilesystemToolKind::Glob => {
                let input = tool_input::<GlobInput>(call)?;
                Ok(vec![self.path_claim(
                    kind.permission_operation(),
                    &model_path(input.path.as_deref().unwrap_or(".")),
                    true,
                )?])
            }
            FilesystemToolKind::Grep => {
                let input = tool_input::<GrepInput>(call)?;
                Ok(vec![self.path_claim(
                    kind.permission_operation(),
                    &model_path(input.path.as_deref().unwrap_or(".")),
                    true,
                )?])
            }
            FilesystemToolKind::Edit => {
                let input = tool_input::<EditInput>(call)?;
                Ok(vec![self.path_claim(
                    kind.permission_operation(),
                    &model_path(&input.file_path),
                    true,
                )?])
            }
            FilesystemToolKind::Write => {
                let input = tool_input::<WriteInput>(call)?;
                Ok(vec![self.path_claim(
                    kind.permission_operation(),
                    &model_path(&input.file_path),
                    false,
                )?])
            }
            FilesystemToolKind::Delete => {
                let input = tool_input::<DeleteInput>(call)?;
                Ok(vec![self.path_claim(
                    kind.permission_operation(),
                    &model_path(&input.file_path),
                    true,
                )?])
            }
            FilesystemToolKind::ApplyPatch => {
                let input = tool_input::<ApplyPatchInput>(call)?;
                self.apply_patch_claims(&input)
            }
        }
    }
    fn path_claim(
        &self,
        operation: PermissionOperation,
        path: &Path,
        existing: bool,
    ) -> Result<PermissionClaim> {
        let resolved = if existing {
            self.resolved_existing_path(path)?
        } else {
            self.resolved_writable_path(path)?
        };
        Ok(PermissionClaim::new(
            operation,
            PermissionResource::FilesystemPath(resolved.clone()),
            format!("{operation:?} filesystem path {}", resolved.display()),
        ))
    }

    fn apply_patch_claims(&self, input: &ApplyPatchInput) -> Result<Vec<PermissionClaim>> {
        if input.patch_text.trim().is_empty() {
            return Err(filesystem_error("patchText cannot be empty"));
        }

        let lines = input.patch_text.lines().collect::<Vec<_>>();
        if lines.first().copied() != Some("*** Begin Patch") {
            return Err(filesystem_error("patch must start with *** Begin Patch"));
        }

        let mut claims = Vec::new();
        let mut index = 1;
        while index < lines.len() {
            let line = lines[index];
            index += 1;
            match line {
                "*** End Patch" => return Ok(claims),
                _ if line.starts_with("*** Add File: ") => {
                    let path = line.trim_start_matches("*** Add File: ");
                    claims.push(self.path_claim(
                        PermissionOperation::Write,
                        &model_path(path),
                        false,
                    )?);
                    skip_patch_body(&lines, &mut index);
                }
                "*** Add File" => {
                    let path = patch_path(&lines, &mut index)?;
                    claims.push(self.path_claim(
                        PermissionOperation::Write,
                        &model_path(&path),
                        false,
                    )?);
                    skip_patch_body(&lines, &mut index);
                }
                _ if line.starts_with("*** Delete File: ") => {
                    let path = line.trim_start_matches("*** Delete File: ");
                    claims.push(self.path_claim(
                        PermissionOperation::Delete,
                        &model_path(path),
                        true,
                    )?);
                }
                "*** Delete File" => {
                    let path = patch_path(&lines, &mut index)?;
                    claims.push(self.path_claim(
                        PermissionOperation::Delete,
                        &model_path(&path),
                        true,
                    )?);
                }
                _ if line.starts_with("*** Update File: ") => {
                    let path = line.trim_start_matches("*** Update File: ");
                    claims.push(self.path_claim(
                        PermissionOperation::Write,
                        &model_path(path),
                        false,
                    )?);
                    skip_patch_body(&lines, &mut index);
                }
                "*** Update File" => {
                    let path = patch_path(&lines, &mut index)?;
                    claims.push(self.path_claim(
                        PermissionOperation::Write,
                        &model_path(&path),
                        false,
                    )?);
                    skip_patch_body(&lines, &mut index);
                }
                _ => return Err(filesystem_error(format!("unsupported patch line: {line}"))),
            }
        }

        Err(filesystem_error("patch missing *** End Patch"))
    }
}
