# Scoped configuration, version 2

User, Workspace and Repo are configuration scopes. A story run is the execution scope. The Rust resolver implements this specification. New CLI runs pin resolved inputs, using repository-only mode by default or explicit invoking-host trust for external scopes; see [run snapshots](run-snapshots.md). References to the original v0.1 CLI below describe historical binaries and pre-snapshot run compatibility. Scope bootstrap/inspect/validate commands remain proposed.

## Compatibility and migration

| Consumer | Repository input | Result |
| --- | --- | --- |
| v0.1 CLI | Valid v1 | Existing command grammar and execution. External scopes are never discovered or applied. |
| v0.1 CLI | v2 | Reject at policy load. |
| v2 resolver | Valid v1 alone | Validate the legacy shape and normalize in memory. Preserve the built-in protection floor, all legacy protected paths and every check in declared order. |
| v2 resolver | Valid v1 with trusted v2 parents | Normalize v1, then apply the same composition rules as for v2. Inherited checks and protections remain mandatory, and conflicting checks fail resolution. |
| v2 resolver | Valid v2 | Apply the v2 rules with any trusted parent scopes. |
| Either | Unsupported version, missing required repo input or malformed required input | Reject. A missing optional user file or no matching workspace is permitted under the discovery rules below. |

Format compatibility does not guarantee identical effective policy with external scopes enabled. A v1 document requires `version`, `protected` and at least one `checks` entry. Retain v0.1 validation, including case-insensitive check IDs, the reserved ID `scope`, nonblank programs and timeouts of 1–3600 seconds. Keep the legacy worker contracts selected by the existing role mapping at their pinned repository paths. Their effective artifact version label is `legacy-v1`. Apply the same agent collision rules to those implicit repo declarations, which carry no override flag. The resolver never rewrites the source. To migrate explicitly, change the version to 2, add `scope: "repo"`, move `protected` and `checks` under `policy`, and declare any required repository agents explicitly. Review path and agent validation before committing the migration.

## Discovery and trust

On Windows the user file is `%APPDATA%/Storyflow/config.json`. On macOS and Linux use `$XDG_CONFIG_HOME/storyflow/config.json` if XDG_CONFIG_HOME is absolute, otherwise `~/.config/storyflow/config.json`. Bootstrap trust from the invoking user's host-provided environment and user-controlled configuration directory. Require an absolute, available base directory controlled by that user and a regular config file inside its canonical Storyflow directory. Reject redirects into a checkout or any location outside that directory. If the trusted location can be established and the optional file is absent, use no user document. An unavailable base or inability to establish trust is an error.

Only that user document may register workspace roots in `workspaces`. Authorization is a separate host input controlled by the invoking user outside every scope document. It identifies exact canonical filesystem roots and authorizes their fixed `storyflow.workspace.json` marker and in-root artifacts. Registration alone grants no authorization. An explicit workspace selection must refer to a registered root. Canonicalize the registered roots, reject duplicates, and establish authorization for every registered root before reading any marker. Reject untrusted roots, missing or invalid registered markers, and unauthorized selections without fallback. A repo or workspace document cannot grant trust. Ignore unregistered markers without reading them.

Workspace markers list explicit repository roots relative to the workspace in `members`. Canonicalize the invoking Git top-level and listed members and compare exact canonical identities under the filesystem's naming rules. Require existing in-root member directories that are Git top-levels. Reject duplicate canonical members per workspace. With no explicit selection, zero matches means no workspace and one match selects it. Multiple matches always fail as ambiguous. An explicit selection must match the unique member claim. Ancestry alone does not imply membership. Only load workspace agent artifacts after membership and trust are established. Repository trust comes from approval of the pinned commit for the run, independently of workspace trust.

## Path namespaces

V2 relative paths are nonempty slash-separated components. Reject empty, dot and parent components, leading or trailing separators, backslashes, colons, control characters and Windows-invalid characters (`< > " | ? *`). Reject components ending in a dot or space, DOS device basenames even with extensions (including COM/LPT digits 1–9 and superscript ¹²³), and ambiguous filesystem aliases such as short-name aliases. Apply these restrictions on every platform. Workspace root registrations must be native absolute paths. Reject drive-relative and device namespace paths and ambiguous aliases before comparing canonical roots.

- **External filesystem.** Resolve member paths against the trusted workspace root and agent sources against the trusted user configuration directory or workspace root. Resolve existing components and links to canonical paths, reject links or reparse points escaping the root even transiently, and require the expected directory or regular file type. Verify containment against the object actually opened so concurrent replacement cannot redirect a read. Compare components rather than string prefixes.
- **Pinned Git tree.** Resolve repository configuration and agents against the exact approved commit's tree, with replacement objects disabled. Match path components by exact UTF-8 spelling. Each intermediate entry must be a tree (mode `040000`). The leaf must be a regular blob (mode `100644` or `100755`). Reject missing entries, directories as artifacts, symlinks, gitlinks, object/type mismatches and invalid UTF-8. Working-tree edits or deletions have no effect.
- **Protected paths.** Validate lexically with no existence check. Compare exact path or slash-boundary descendants after Unicode lowercase conversion, as the v0.1 gate does. Never drop built-in protections. Carry legacy protected strings through using legacy validation and matching. Future enforcement must fail closed for candidate paths with ambiguous platform spellings, including case or normalization collisions, short-name aliases and invalid components, before applying scope or protection checks. It must also reject aliases against the existing baseline or checkout namespace. Protection applies to paths created after initialization.

## Document shape

[The JSON Schema](config.schema.json) defines v2 structure. Every document requires `version: 2` and `scope` equal to `user`, `workspace` or `repo`, matching its source. Only user documents accept `workspaces`. Only workspace documents accept and require `members`. All scopes may declare `agents`, `models`, `policy` and `tools`. Reject unknown fields, nulls, wrong types and duplicate raw JSON keys. Diagnostics identify the source and field path, and identify both origins for conflicts.

- `agents` entries require `id`, `source` and `version`. IDs match lowercase ASCII letters, digits and hyphens, starting with a letter. Source is a relative path with a lowercase `.md` suffix. Version is a nonempty artifact label. Only workspace and repo entries accept optional boolean `override`.
- `models` accepts optional nonempty string hints for `economy`, `standard` and `high`.
- `policy` accepts optional `protected` and `checks`. Each check requires the v1 fields `id`, `program`, `args` and `timeout_seconds` and uses the legacy check constraints. Every check is mandatory.
- `tools` accepts optional `allow` and `deny` lists of distinct capability IDs using agent ID syntax. Reject IDs unknown to the host.

## Composition and provenance

Resolve User → Workspace → Repo. Model hints override per tier. Duplicate agent IDs within one document always fail. Omitted `override` and `false` have the same meaning. With no inherited agent of the same ID, add the declaration unless `override: true` is present, which fails with `missing_override_target`. With an inherited agent, identical version and exact Markdown bytes deduplicate and keep the inherited provenance when the flag is absent or false. Otherwise require `override: true` for replacement. A true flag targets the same ID in the nearest effective parent and selects the complete new artifact, even if identical. Repo wins over Workspace, and Workspace over User, only after each declaration has passed validation. Conflicting definitions without the flag fail as `agent_conflict`. Markdown is never combined.

Each accepted artifact must come from its trusted source. Markdown has no authority over the composed policy. Dispatch and enforcement must keep the effective capability ceiling, path restrictions and gate requirements regardless of contract text. A proposed operation that requests additional capability, writes into protected paths or skips a mandatory check must fail closed. Where the host cannot enforce these boundaries, it must refuse the run. This contract requires no interpretation of Markdown as a policy language.

Protected paths and mandatory checks accumulate. Compare check IDs using ASCII lowercase. Identical repeated check definitions across scopes deduplicate at their first position, comparing program, ordered args and timeout exactly. Reject duplicate IDs within one document and conflicting definitions across scopes. Run checks in first-declaration order in repository context. The resulting configuration must provide at least one declared mandatory check as well as the built-in scope gate.

Start with host-granted capabilities. At each scope, reject explicit allow entries outside the incoming grants, intersect with any explicit allow list and subtract that scope's denies. Omitted lists impose no constraint. An allow and deny at the same scope resolves to deny. A child cannot re-grant a denied capability.

Record each effective agent's ID, artifact version, declaring scope, source path and SHA-256 over the exact consumed Markdown bytes. Future initialization must snapshot effective policy and artifact bytes with hashes and the pinned commit before dispatch. Later external changes must not change the inputs of an existing run. Unsupported, malformed, ambiguous or unauthorized inputs stop resolution before dispatch.

The [schema-1 run snapshot](run-snapshots.md) now implements initialization/replay pinning. New-run protection and candidate namespace checks conservatively normalize canonical Unicode and case aliases; the original `protected_matches` API and pre-snapshot runs retain their documented v0.1 matching. Active-run migration/rebind remains explicitly unsupported.

## Command grammar

`storyflow init ID [BASE]` initializes a story run against the approved Git baseline. The proposed `storyflow init --scope user|workspace|repo [--path PATH]` bootstraps a configuration file without initializing a run or overwriting an existing file. The forms are mutually exclusive. Bootstrapping a workspace writes its marker. Registration and authorization are separate steps.

## Specification validation

From the repository root run `pwsh -NoProfile -File docs/config-fixtures/validate.ps1` with PowerShell 7.6. The script checks declared schema expectations for v2 documents and parser rejection for malformed JSON. Fixtures tagged `resolver-pending` specify inputs and expected results for future regression tests. Their v2 documents are checked for shape, but their resolver outcomes are pending. These checks do not exercise raw duplicate keys, v1 normalization, conflicts, trust, discovery, Git object selection or enforcement. Fixture `/fixture` roots are abstract absolute paths to map into a temporary test filesystem.

Review validation on PowerShell 7.6.9 yielded 3 schema accepts, 12 schema rejects and 1 parser rejection. The 30 pending resolver scenarios include 46 v2 input documents that passed shape validation.
