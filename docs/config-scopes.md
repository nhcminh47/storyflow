# Scoped configuration, version 2

This contract defines inputs for a future resolver. The current CLI reads version 1 of the repository's `storyflow.json` from the pinned Git baseline.

| Input | Compatibility |
| --- | --- |
| Repository `storyflow.json`, version 1 | Keep the existing run grammar and policy. Treat every existing check as mandatory. |
| Repository `storyflow.json`, version 2 | Requires a version 2 resolver. Migrate by adding `scope: "repo"` and moving `protected` and `checks` under `policy`. |
| Missing user or workspace config | Resolve with the scopes that are present. |
| Missing repo config, unsupported version, or malformed scope | Fail resolution before dispatch. |

## Discovery

On Windows, the user file is `%APPDATA%/Storyflow/config.json`. On macOS and Linux it is `$XDG_CONFIG_HOME/storyflow/config.json` when XDG_CONFIG_HOME is absolute, otherwise `~/.config/storyflow/config.json`. An unavailable home/config directory is a discovery error. Paths use the invoking user's environment.

The optional user `workspaces` array holds explicitly registered absolute roots. A registered root must contain `storyflow.workspace.json`, which lists member repositories in `members` as paths relative to that root. Resolve the repository's canonical Git top-level directory and match exact canonical member paths. Zero matches means no workspace, one selects it, and multiple matches are an ambiguity error. Duplicate canonical roots or members are errors. Directory ancestry alone grants no membership. Missing markers for registered roots are errors. Unregistered markers are ignored.

All relative config paths use forward slashes. Reject empty, dot and parent components, absolute or drive-relative paths, and platform aliases such as Windows device names. Canonicalize existing roots, members and agent files before comparing or reading them. Reject links that lead outside the declaring root. Normalize protected paths against the repository root, including paths that do not exist yet. Repository config and repo agent artifacts are read from the approved Git baseline. Require explicit trust for external scope roots before reading their files. An explicitly selected or registered untrusted scope fails resolution.

## Document shape

Every version 2 file is a JSON object with required `version: 2` and `scope` equal to `user`, `workspace`, or `repo`. Optional keys are `agents`, `models`, `policy`, and `tools`. Only user files may contain `workspaces`. Workspace files require `members`. Reject duplicate JSON keys, unknown keys, nulls and wrong types. Report file and field path for structural errors and both origins for conflicts.

- `agents` is an array of objects with required `id`, `source`, and `version` strings. IDs are unique within a document and match lowercase ASCII letters, digits and hyphens, starting with a letter. Source is a root-relative Markdown file. Version is a nonempty artifact version label.
- `models` maps optional `economy`, `standard`, and `high` keys to nonempty model hint strings.
- `policy` accepts optional `protected` and `checks`. Protected paths are repository-relative strings. Checks use the v1 fields `id`, `program`, `args`, and `timeout_seconds`, with unique nonempty IDs and positive integer timeouts. Every check is mandatory.
- `tools` accepts optional `allow` and `deny` arrays of unique capability IDs, with the same syntax as agent IDs. Unknown capability IDs fail resolution against the runtime's supported capabilities.

## Composition

Resolve user, workspace, then repo. Model hints override per tier. Agent IDs are additive. A repeated ID across scopes is accepted only when the version and Markdown contents are identical. Different contents or versions are conflicts. The agent artifact is always a complete Markdown file.

Protected paths and mandatory checks accumulate. An identical repeated check is accepted. Changing an existing check's command, arguments or timeout is a conflict. Command paths and arguments are interpreted in repository context when the gate runs.

Start from the capabilities granted by the host. Intersect with each explicit allow list and subtract the union of all denies. Reject an explicit allow entry outside the incoming grants. An omitted list imposes no new constraint. Configuration cannot grant host permissions.

## Command grammar

`storyflow init ID [BASE]` initializes a story run against the approved Git baseline. The proposed `storyflow init --scope user|workspace|repo [--path PATH]` bootstraps a configuration file without initializing a run or overwriting an existing file. The positional and scope forms are mutually exclusive. Bootstrapping a workspace writes its marker. Registration in the user configuration requires a separate explicit edit.

The fixtures contain proposed inputs and expected outcomes. Fixture paths use /fixture as an abstract absolute root, mapped to a temporary absolute path by future resolver tests.
