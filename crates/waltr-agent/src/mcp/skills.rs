use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::{LlmError, Result};

pub(super) const METADATA_BUDGET: usize = 16 * 1024 * 1024;

pub(super) fn charge_metadata(value: &impl serde::Serialize, remaining: &mut usize) -> Result<()> {
    struct BudgetWriter<'a>(&'a mut usize);
    impl std::io::Write for BudgetWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > *self.0 {
                return Err(std::io::Error::other(
                    "MCP skill metadata exceeds byte budget",
                ));
            }
            *self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(BudgetWriter(remaining), value).map_err(|error| LlmError::Protocol {
        provider: "mcp-skills",
        message: format!("MCP skill metadata exceeds serialized byte budget: {error}"),
    })
}

pub(crate) fn verified_resource_bytes(
    skill: &mcp::skills::McpSkill,
    uri: &str,
    result: mcp::McpResourceResult,
) -> Result<Vec<u8>> {
    let invalid = || LlmError::Protocol {
        provider: "mcp-skills",
        message:
            "skill resource must contain exactly one matching URI and either text or base64 blob"
                .into(),
    };
    let contents = result
        .raw
        .get("contents")
        .and_then(serde_json::Value::as_array)
        .filter(|contents| contents.len() == 1)
        .ok_or_else(invalid)?;
    let content = &contents[0];
    if content.get("uri").and_then(serde_json::Value::as_str) != Some(uri) {
        return Err(invalid());
    }
    let bytes = match (content.get("text"), content.get("blob")) {
        (Some(text), None) => text.as_str().ok_or_else(invalid)?.as_bytes().to_vec(),
        (None, Some(blob)) => STANDARD
            .decode(blob.as_str().ok_or_else(invalid)?)
            .map_err(|_| invalid())?,
        _ => return Err(invalid()),
    };
    mcp::skills::verify_skill_bytes(skill, uri, &bytes).map_err(|error| LlmError::Protocol {
        provider: "mcp-skills",
        message: format!("Held skill integrity check failed: {error}. Refresh and obtain user reapproval; the held manifest will not be substituted."),
    })?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp::skills::{McpSkillResources, SkillCatalog};
    use serde_json::json;

    const ENTRY: &[u8] =
        b"---\nname: inspect\ndescription: Inspect safely.\n---\n@tool(bash) is literal.\n";

    #[test]
    fn mcp_skills_direct_metadata_budget_counts_complete_serialized_manifest() {
        let catalog = SkillCatalog::new()
            .with_skill("ops/inspect", &[("SKILL.md", ENTRY)])
            .unwrap();
        let mut result = catalog.get("skill://ops/inspect/SKILL.md").unwrap();
        result
            .skill
            .frontmatter
            .insert("extra".into(), json!("x".repeat(1024)));
        let size = serde_json::to_vec(&result).unwrap().len();
        let mut exact = size;
        charge_metadata(&result, &mut exact).unwrap();
        assert_eq!(exact, 0);
        assert!(
            charge_metadata(&result, &mut (size - 1))
                .unwrap_err()
                .to_string()
                .contains("byte budget")
        );
    }

    #[test]
    fn mcp_skills_decode_text_and_blob_without_normalization() {
        let catalog = SkillCatalog::new()
            .with_skill(
                "ops/inspect",
                &[("SKILL.md", ENTRY), ("data.bin", &[0, 255])],
            )
            .unwrap();
        let skill = catalog.get("skill://ops/inspect/SKILL.md").unwrap().skill;
        assert_eq!(
            verified_resource_bytes(&skill, &skill.uri, catalog.read(&skill.uri).unwrap()).unwrap(),
            ENTRY
        );
        let uri = "skill://ops/inspect/data.bin";
        assert_eq!(
            verified_resource_bytes(&skill, uri, catalog.read(uri).unwrap()).unwrap(),
            [0, 255]
        );
    }

    #[test]
    fn mcp_skills_reject_ambiguous_and_mismatched_transport_contents() {
        let catalog = SkillCatalog::new()
            .with_skill("ops/inspect", &[("SKILL.md", ENTRY)])
            .unwrap();
        let skill = catalog.get("skill://ops/inspect/SKILL.md").unwrap().skill;
        for raw in [
            json!({"contents":[]}),
            json!({"contents":[{"uri":skill.uri,"text":"x"},{"uri":skill.uri,"text":"y"}]}),
            json!({"contents":[{"uri":"skill://other/SKILL.md","text":String::from_utf8_lossy(ENTRY)}]}),
            json!({"contents":[{"uri":skill.uri,"text":String::from_utf8_lossy(ENTRY),"blob":STANDARD.encode(ENTRY)}]}),
            json!({"contents":[{"uri":skill.uri,"blob":"not base64"}]}),
        ] {
            assert!(
                verified_resource_bytes(&skill, &skill.uri, mcp::McpResourceResult { raw })
                    .is_err()
            );
        }
    }

    #[test]
    fn mcp_skills_reject_size_digest_frontmatter_and_unlisted_resources() {
        let catalog = SkillCatalog::new()
            .with_skill("ops/inspect", &[("SKILL.md", ENTRY)])
            .unwrap();
        let skill = catalog.get("skill://ops/inspect/SKILL.md").unwrap().skill;
        let content = catalog.read(&skill.uri).unwrap();
        let mut held = skill.clone();
        let McpSkillResources::Files(files) = &mut held.resources else {
            panic!()
        };
        files[0].size += 1;
        assert!(verified_resource_bytes(&held, &held.uri, content.clone()).is_err());
        let mut digest = skill.clone();
        let McpSkillResources::Files(files) = &mut digest.resources else {
            panic!()
        };
        files[0].digest = format!("sha256:{}", "0".repeat(64));
        assert!(verified_resource_bytes(&digest, &digest.uri, content.clone()).is_err());
        let mut frontmatter = skill.clone();
        frontmatter
            .frontmatter
            .insert("description".into(), json!("Different metadata."));
        assert!(verified_resource_bytes(&frontmatter, &frontmatter.uri, content.clone()).is_err());
        assert!(
            verified_resource_bytes(&skill, "skill://ops/inspect/not-listed", content).is_err()
        );
    }

    #[test]
    fn mcp_skills_stale_bytes_do_not_substitute_held_manifest() {
        let old = SkillCatalog::new()
            .with_skill("ops/inspect", &[("SKILL.md", ENTRY)])
            .unwrap();
        let new = SkillCatalog::new()
            .with_skill(
                "ops/inspect",
                &[(
                    "SKILL.md",
                    b"---\nname: inspect\ndescription: Inspect safely.\n---\nChanged body.\n",
                )],
            )
            .unwrap();
        let held = old.get("skill://ops/inspect/SKILL.md").unwrap().skill;
        let snapshot = held.clone();
        let err =
            verified_resource_bytes(&held, &held.uri, new.read(&held.uri).unwrap()).unwrap_err();
        assert!(err.to_string().contains("reapproval"));
        assert_eq!(held, snapshot);
    }
}
