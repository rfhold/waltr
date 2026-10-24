# MCP Skills

The generic `mcp` crate implements the official `io.modelcontextprotocol/skills` extension.
Waltr's session activation and permissions belong in [agent MCP integration](../agent/mcp.md#mcp-skills).

## Protocol Surface

| API | Behavior |
| --- | --- |
| `McpClientOptions::with_skills` | Explicit client opt-in; default off. |
| `McpClient::list_skills` | Retrieves one typed `skills/list` page with an optional cursor. |
| `McpClient::get_skill` | Retrieves one typed `skills/get` entry by exact URI. |
| `ServerHandler::skill_catalog` | Supplies an optional immutable catalog. |
| `SkillCatalog::with_skill` | Registers an explicit complete file list under one skill root. |
| `verify_skill_bytes` | Checks manifest membership, raw length, SHA-256 digest, and entrypoint frontmatter equality. |

Client methods require both client opt-in and server support. Result pages carry `resultType: "complete"`, `ttlMs`, and `cacheScope`.
Cache metadata controls freshness and sharing, not integrity or trust.
The embedded catalog returns private, zero-TTL results and deterministic pages of at most 32 entries.
The crate does not implement `resources/directory/read` or advertise `directoryRead` support.

## Embedded Catalogs

Both `#[mcp::mcp_server]` and `#[mcp::progressive_server]` accept `skills = self.catalog`.
The expression supplies an `Arc<SkillCatalog>`; the generated handler clones this shared catalog.
A nonempty catalog advertises Skills and resource support.

```rust
let catalog = mcp::SkillCatalog::new().with_skill(
    "release-desk/inspect-release",
    &[
        ("SKILL.md", include_bytes!("skills/inspect-release/SKILL.md")),
        (
            "references/actions.md",
            include_bytes!("skills/inspect-release/references/actions.md"),
        ),
    ],
)?;
```

Authors supply every file explicitly, including nested files. The catalog performs no runtime filesystem scan.
The example publishes `skill://release-desk/inspect-release/SKILL.md` and its supporting resource.
The skill root's final component must equal the frontmatter `name`.
The macros do not generate workflow prose or mechanical reference files.

## Manifest And Bytes

Each entry contains an absolute `/SKILL.md` URI, a frontmatter object, and either a complete `resources` array or the `"dynamic"` marker.
Each file has an exact URI, `sha256:` digest with lowercase hexadecimal digits, and raw byte size.
The complete manifest includes `SKILL.md`; all files stay inside its root.
Validation rejects duplicate URIs, unsafe paths, malformed digests, missing entrypoints, and mismatched root names.
Limits are 512 files and 16 MiB aggregate raw bytes per skill.

Frontmatter requires a valid `name` and nonempty `description`. The parser preserves author-defined fields as JSON values.
It rejects YAML that cannot retain its values in JSON, including tagged values and non-string mapping keys.
`SKILL.md` must contain UTF-8 YAML frontmatter.

The [Agent Skills specification](https://agentskills.io/specification#metadata-field) expects `metadata` to map string keys to string values.
Some servers publish arrays and other JSON values instead.
For compatibility, this crate accepts a string-keyed `metadata` object with arbitrary JSON values: strings, arrays, numbers, booleans, null, and nested objects.
It preserves those values without coercion and rejects non-object `metadata` roots.
The parser's lossless JSON checks and raw-byte integrity checks still apply.

The catalog preserves original bytes. `resources/read` returns UTF-8 text without normalization, or Base64 for binary files.
Verification applies to decoded raw bytes, not transport encoding.
Generic transport limits still apply; callers choose explicit limits for larger resources.
Generic types represent dynamic manifests, but byte verification rejects them.

## Authority

A skill's identity includes its server origin and URI, not its display name alone.
Discovery metadata and ordinary resource reads do not activate instructions or establish user approval.
Digests establish byte integrity, not author trust.
Frontmatter, `allowed-tools`, and hooks confer no permissions and trigger no automatic host script execution.
The host owns activation, consent, origin labels, and execution policy.

## References

- [Skills implementation](../../crates/mcp/src/skills.rs)
- [Generic Skills tests](../../crates/mcp/tests/skills.rs)
- [Revision-scoped Skills tests](../../crates/mcp/tests/spec/v2026_07_28/skills.rs)
- [Progressive servers](progressive.md)
