//! Immutable Skills catalog and untrusted manifest verification contracts.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use mcp::{
    McpSkillResources, SkillCatalog,
    skills::{MAX_SKILL_BYTES, parse_skill_frontmatter, validate_skill, verify_skill_bytes},
};
use serde_json::json;

const ENTRY: &[u8] = b"---\r\nname: inspect\r\ndescription: Inspect bounded state.\r\ncustom: [1, true, opaque]\r\nmetadata: {owner: ops}\r\n---\r\nExact body\r\n";

#[test]
fn catalog_snapshots_exact_bytes_and_complete_manifests() {
    let mut binary = vec![0, 255, 10];
    let catalog = SkillCatalog::new()
        .with_skill(
            "ops/inspect",
            &[
                ("SKILL.md", ENTRY),
                ("assets/data.bin", &binary),
                ("references/actions.json", b"{\"action\":\"inspect\"}"),
            ],
        )
        .unwrap();
    binary[1] = 0;
    let skill = catalog.get("skill://ops/inspect/SKILL.md").unwrap().skill;
    assert_eq!(skill.frontmatter["custom"], json!([1, true, "opaque"]));
    let McpSkillResources::Files(files) = &skill.resources else {
        panic!("static manifest")
    };
    assert_eq!(files.len(), 3);
    let binary = files
        .iter()
        .find(|file| file.uri.ends_with("data.bin"))
        .unwrap();
    assert_eq!(binary.size, 3);
    assert_eq!(
        binary.digest,
        "sha256:712450d3c4a79eea9509e75dc1dacdeff58034df538536cfae2da882bd8a0c50"
    );
    for file in files {
        let result = catalog.read(&file.uri).unwrap();
        let content = &result.raw["contents"][0];
        let bytes = if let Some(text) = content["text"].as_str() {
            text.as_bytes().to_vec()
        } else {
            STANDARD.decode(content["blob"].as_str().unwrap()).unwrap()
        };
        verify_skill_bytes(&skill, &file.uri, &bytes).unwrap();
    }
    assert_eq!(
        catalog.read(&skill.uri).unwrap().raw["contents"][0]["text"]
            .as_str()
            .unwrap()
            .as_bytes(),
        ENTRY
    );
    assert_eq!(
        catalog
            .read("skill://ops/inspect/assets/data.bin")
            .unwrap()
            .raw["contents"][0]["blob"],
        "AP8K"
    );
    assert!(catalog.read("skill://ops/inspect/missing").is_none());
    assert!(catalog.get("skill://ops/missing/SKILL.md").is_err());
}

#[test]
fn catalog_rejects_traversal_duplicates_and_bad_frontmatter() {
    for path in [
        "",
        "/inspect",
        "ops/../inspect",
        "ops/./inspect",
        "ops//inspect",
        "ops\\inspect",
        "ops/%2e%2e/inspect",
        "ops/other",
    ] {
        assert!(
            SkillCatalog::new()
                .with_skill(path, &[("SKILL.md", ENTRY)])
                .is_err(),
            "{path}"
        );
    }
    for path in ["../x", "/x", "./x", "assets//x", "a\\x", "%2fetc"] {
        assert!(
            SkillCatalog::new()
                .with_skill("ops/inspect", &[("SKILL.md", ENTRY), (path, b"x")])
                .is_err(),
            "{path}"
        );
    }
    assert!(
        SkillCatalog::new()
            .with_skill("ops/inspect", &[("SKILL.md", ENTRY), ("SKILL.md", ENTRY)])
            .is_err()
    );
    assert!(
        SkillCatalog::new()
            .with_skill("ops/inspect", &[("references/SKILL.md", ENTRY)])
            .is_err()
    );
    let catalog = SkillCatalog::new()
        .with_skill("ops/inspect", &[("SKILL.md", ENTRY)])
        .unwrap();
    assert!(
        catalog
            .with_skill("ops/inspect", &[("SKILL.md", ENTRY)])
            .is_err()
    );
    for yaml in [
        "name: Inspect\ndescription: x",
        "name: in--spect\ndescription: x",
        "name: inspect",
        "name: inspect\ndescription: ''",
        "name: inspect\ndescription: [x]",
        "name: inspect\ndescription: x\nmetadata: scalar",
        "name: inspect\ndescription: x\nmetadata: 1",
        "name: inspect\ndescription: x\nmetadata: true",
        "name: inspect\ndescription: x\nmetadata: [ops]",
        "name: inspect\ndescription: x\nmetadata: null",
        "name: inspect\ndescription: x\ncompatibility: ''",
        "name: inspect\nname: other\ndescription: x",
    ] {
        assert!(
            parse_skill_frontmatter(format!("---\n{yaml}\n---\n").as_bytes()).is_err(),
            "{yaml}"
        );
    }
    assert!(parse_skill_frontmatter(b"name: inspect").is_err());
    assert!(parse_skill_frontmatter(b"---\nname: inspect").is_err());
    for extension in [
        "custom: .nan",
        "custom: {1: value}",
        "custom: !tag value",
        "metadata: {nested: .nan}",
        "metadata: {nested: {1: value}}",
        "metadata: {nested: !tag value}",
    ] {
        assert!(
            parse_skill_frontmatter(
                format!("---\nname: inspect\ndescription: x\n{extension}\n---\n").as_bytes()
            )
            .is_err()
        );
    }
}

// Agent Skills expects metadata strings; compatibility with servers requires JSON values.
#[test]
fn metadata_json_values_are_preserved_without_weakening_integrity() {
    for metadata in [
        "{}",
        "{owner: ops}",
        "{owner: 1, tags: [infra, ops], score: 1.5, enabled: true, missing: null, nested: {items: [false, null, {key: value}]}}",
    ] {
        let entry = format!(
            "---\r\nname: inspect\r\ndescription: Inspect state.\r\nmetadata: {metadata}\r\n---\r\nExact body\r\n"
        );
        let expected: serde_json::Value = serde_yaml_ng::from_str(metadata).unwrap();
        let parsed = parse_skill_frontmatter(entry.as_bytes()).unwrap();
        assert_eq!(parsed["metadata"], expected);
        let catalog = SkillCatalog::new()
            .with_skill("ops/inspect", &[("SKILL.md", entry.as_bytes())])
            .unwrap();
        let skill = catalog.get("skill://ops/inspect/SKILL.md").unwrap().skill;
        assert_eq!(skill.frontmatter, parsed);
        assert_eq!(catalog.list(None).unwrap().skills, vec![skill.clone()]);
        let read = catalog.read(&skill.uri).unwrap();
        let bytes = read.raw["contents"][0]["text"].as_str().unwrap().as_bytes();
        assert_eq!(bytes, entry.as_bytes());
        verify_skill_bytes(&skill, &skill.uri, bytes).unwrap();
        let mut changed = skill.clone();
        changed.frontmatter["metadata"] = json!({"tampered": true});
        assert!(verify_skill_bytes(&changed, &changed.uri, bytes).is_err());
        let mut tampered = bytes.to_vec();
        *tampered.last_mut().unwrap() = b'!';
        assert!(verify_skill_bytes(&skill, &skill.uri, &tampered).is_err());
        for invalid in [
            json!("scalar"),
            json!(1),
            json!(true),
            json!([]),
            json!(null),
        ] {
            let mut changed = skill.clone();
            changed.frontmatter["metadata"] = invalid;
            assert!(validate_skill(&changed).is_err());
        }
    }
}

#[test]
fn client_verification_rejects_manifest_and_metadata_tampering() {
    let skill = SkillCatalog::new()
        .with_skill("ops/inspect", &[("SKILL.md", ENTRY)])
        .unwrap()
        .get("skill://ops/inspect/SKILL.md")
        .unwrap()
        .skill;
    verify_skill_bytes(&skill, &skill.uri, ENTRY).unwrap();
    assert!(verify_skill_bytes(&skill, &skill.uri, b"tampered").is_err());
    assert!(verify_skill_bytes(&skill, "skill://other/SKILL.md", ENTRY).is_err());
    let mut changed = skill.clone();
    changed
        .frontmatter
        .insert("custom".into(), json!("changed"));
    assert!(verify_skill_bytes(&changed, &changed.uri, ENTRY).is_err());
    for mutation in [
        "duplicate",
        "digest",
        "size",
        "outside",
        "traversal",
        "encoded-traversal",
        "missing",
    ] {
        let mut changed = skill.clone();
        let McpSkillResources::Files(files) = &mut changed.resources else {
            unreachable!()
        };
        match mutation {
            "duplicate" => files.push(files[0].clone()),
            "digest" => files[0].digest = format!("sha256:{}", "A".repeat(64)),
            "size" => files[0].size = MAX_SKILL_BYTES + 1,
            "outside" => files[0].uri = "skill://other/SKILL.md".into(),
            "traversal" => files[0].uri = "skill://ops/inspect/../SKILL.md".into(),
            "encoded-traversal" => files[0].uri = "skill://ops/inspect/%2e%2e/SKILL.md".into(),
            "missing" => files.clear(),
            _ => unreachable!(),
        }
        assert!(validate_skill(&changed).is_err(), "{mutation}");
    }
    let mut dynamic = skill.clone();
    dynamic.resources = serde_json::from_value(json!("dynamic")).unwrap();
    validate_skill(&dynamic).unwrap();
    assert!(verify_skill_bytes(&dynamic, &dynamic.uri, ENTRY).is_err());
    assert!(serde_json::from_value::<McpSkillResources>(json!("anything-else")).is_err());
    for malformed in [
        json!({"resultType":"complete", "skills":[], "ttlMs":-1,"cacheScope":"private"}),
        json!({"resultType":"complete", "skills":[], "ttlMs":1,"cacheScope":"global"}),
        json!({"skills":[], "ttlMs":1,"cacheScope":"private"}),
        json!({"resultType":"complete", "skills":[], "cacheScope":"private"}),
    ] {
        assert!(serde_json::from_value::<mcp::McpSkillList>(malformed).is_err());
    }
}

#[test]
fn pagination_and_maximum_file_byte_boundaries_are_supported() {
    let mut catalog = SkillCatalog::new();
    for index in 0..35 {
        let name = format!("skill-{index:02}");
        let entry = format!("---\nname: {name}\ndescription: x\n---\n");
        catalog = catalog
            .with_skill(&format!("ops/{name}"), &[("SKILL.md", entry.as_bytes())])
            .unwrap();
    }
    let first = catalog.list(None).unwrap();
    assert_eq!(first.skills.len(), 32);
    let second = catalog.list(first.next_cursor).unwrap();
    assert_eq!(second.skills.len(), 3);
    assert!(second.next_cursor.is_none());
    for cursor in ["", "skills:0", "skills:35", "other:32", "skills:-1"] {
        assert!(catalog.list(Some(cursor.into())).is_err());
    }
    let names = (0..511).map(|n| format!("files/{n}")).collect::<Vec<_>>();
    let mut files = vec![("SKILL.md", ENTRY)];
    files.extend(names.iter().map(|name| (name.as_str(), b"".as_slice())));
    assert!(
        SkillCatalog::new()
            .with_skill("ops/inspect", &files)
            .is_ok()
    );
    files.push(("overflow", b""));
    assert!(
        SkillCatalog::new()
            .with_skill("ops/inspect", &files)
            .is_err()
    );
    let bytes = vec![0; MAX_SKILL_BYTES as usize - ENTRY.len()];
    assert!(
        SkillCatalog::new()
            .with_skill("ops/inspect", &[("SKILL.md", ENTRY), ("data", &bytes)])
            .is_ok()
    );
    assert!(
        SkillCatalog::new()
            .with_skill(
                "ops/inspect",
                &[("SKILL.md", ENTRY), ("data", &bytes), ("overflow", b"x")]
            )
            .is_err()
    );
}

#[test]
fn nested_publications_must_share_identical_complete_snapshots() {
    let nested = b"---\nname: child\ndescription: Child.\n---\n";
    let catalog = SkillCatalog::new()
        .with_skill(
            "ops/inspect",
            &[("SKILL.md", ENTRY), ("child/SKILL.md", nested)],
        )
        .unwrap();
    assert!(
        catalog
            .clone()
            .with_skill("ops/inspect/child", &[("SKILL.md", nested)])
            .is_ok()
    );
    assert!(
        catalog
            .with_skill("ops/inspect/child", &[("SKILL.md", nested), ("new", b"x")])
            .is_err()
    );
}

#[test]
fn embedded_supporting_paths_are_uri_encoded_without_changing_bytes() {
    let catalog = SkillCatalog::new()
        .with_skill(
            "ops team/inspect",
            &[("SKILL.md", ENTRY), ("assets/an example.txt", b"exact")],
        )
        .unwrap();
    let skill = catalog
        .get("skill://ops%20team/inspect/SKILL.md")
        .unwrap()
        .skill;
    let uri = "skill://ops%20team/inspect/assets/an%20example.txt";
    assert_eq!(
        catalog.read(uri).unwrap().raw["contents"][0]["text"],
        "exact"
    );
    verify_skill_bytes(&skill, uri, b"exact").unwrap();
}

#[test]
fn extension_capabilities_and_alternate_resource_schemes_are_typed() {
    for value in [
        json!({"resources":{}, "extensions":{"io.modelcontextprotocol/skills":{}}}),
        json!({"resources":{}, "extensions":{"io.modelcontextprotocol/skills":{"directoryRead":true}}}),
    ] {
        let parsed = mcp::McpServerCapabilities::parse(Some(&value));
        assert!(parsed.supports_skills());
        assert_eq!(
            parsed.skills().unwrap().directory_read,
            value["extensions"]["io.modelcontextprotocol/skills"]["directoryRead"]
                .as_bool()
                .unwrap_or(false)
        );
    }
    for value in [
        json!({"extensions":{"io.modelcontextprotocol/skills":{}}}),
        json!({"resources":{}, "extensions":{"io.modelcontextprotocol/skills":true}}),
        json!({"resources":{}, "extensions":{"io.modelcontextprotocol/skills":{"directoryRead":"yes"}}}),
    ] {
        assert!(!mcp::McpServerCapabilities::parse(Some(&value)).supports_skills());
    }
    assert_eq!(
        serde_json::to_value(mcp::McpSkillsCapability::default()).unwrap(),
        json!({})
    );
    let mut skill = SkillCatalog::new()
        .with_skill("ops/inspect", &[("SKILL.md", ENTRY)])
        .unwrap()
        .get("skill://ops/inspect/SKILL.md")
        .unwrap()
        .skill;
    for uri in [
        "github://owner/repo/inspect/SKILL.md",
        "file:///workspace/inspect/SKILL.md",
        "file:/workspace/inspect/SKILL.md",
        "https://example.test:443/inspect/SKILL.md",
    ] {
        skill.uri = uri.into();
        let McpSkillResources::Files(files) = &mut skill.resources else {
            unreachable!()
        };
        files[0].uri = uri.into();
        validate_skill(&skill).unwrap();
        verify_skill_bytes(&skill, uri, ENTRY).unwrap();
    }
}
