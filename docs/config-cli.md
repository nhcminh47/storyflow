# Configuration bootstrap and inspection CLI

`storyflow init --scope user|workspace|repo [--path DIRECTORY]` creates a version-2 configuration skeleton without creating a story run. The flag form and `storyflow init ID [BASE]` are mutually exclusive. Each flag is accepted once; unknown flags, missing values and mixed positional arguments return exit 2 before opening a journal. All new commands accept `--help`.

`--path` is a directory, absolute or relative to the current directory. Without it:

| Scope | Target |
| --- | --- |
| User | `%APPDATA%/Storyflow/config.json` on Windows; absolute `$XDG_CONFIG_HOME/storyflow/config.json` or `$HOME/.config/storyflow/config.json` on Linux |
| Workspace | `storyflow.workspace.json` in the current directory |
| Repo | `storyflow.json` at the current Git top-level |

Bootstrap creates missing physical directories and uses exclusive file creation. An existing regular file is returned as `status: "unchanged"` without reading, validating or rewriting it, including hand-edited or invalid JSON. Directory targets, parent traversal, symlink/reparse redirects and unsafe portable path components fail closed. User bootstrap refuses locations inside Git checkouts. Windows holds ancestor handles denying write/delete sharing; Linux creates/traverses through directory descriptors with no-follow operations. Secure bootstrap on other platforms is currently unsupported. A failed storage write can leave an incomplete newly created file; inspect it before retrying, because bootstrap will preserve it.

User skeletons contain an empty workspace registry; Workspace skeletons contain an empty member list. Neither registers or authorizes a workspace. Repo skeletons deliberately contain no agents or executable checks: add reviewed contracts and mandatory project checks, then commit them before effective validation or story initialization. Bootstrap never invents a passing gate, migrates an existing document or grants host trust.

```sh
storyflow init --scope workspace --path ./product-workspace
storyflow init --scope repo
storyflow config inspect --effective
storyflow config validate
```

Inspection and validation resolve the current repository's exact committed `HEAD`, with repository-only resolution by default. Uncommitted configuration/contract edits are intentionally ignored. Commit proposed configuration to a topic branch to inspect it. These commands neither execute declared checks nor create/mutate `.storyflow` state, pin refs, rebind active runs or dispatch workers. They report current configuration, not an existing run's saved snapshot; use `status`/`audit` for a run.

To include external scopes, supply the same explicit `STORYFLOW_HOST_CONTEXT` and independently authorized `STORYFLOW_HOST_CONTEXT_ROOT` inputs used at run initialization. Both must be outside every Git checkout; the existing secure host-context reader and resolver enforce registration, canonical identity and authorization. Missing, invalid or untrusted input fails without repository-only fallback. Bootstrap does not supply these trust inputs. See [run snapshots](run-snapshots.md) for the host manifest format.

Successful commands return versioned JSON (`schema: 1`) and exit 0. Configuration and grammar failures return a JSON `error` with a stable `code` and constant `message`, plus `origin` and `related` origins when supplied by the resolver, and exit 2. Validation checks strict schema, artifact availability, trust, composition and mandatory policy floors. Valid reduced capability profiles can be inspected/validated; this is not admission to execute them. Existing story initialization still refuses profiles the host cannot enforce.

Inspection returns scope identities/baseline, protected values and origins, built-in gate, check IDs/timeouts/origins, contract versions/digests/origins, capabilities and provenance. Model values, check programs/arguments and complete contract content are always represented as `[redacted]`. No command output substitutes for secure secret storage: keep secrets out of paths, IDs, versions, configuration and reports. `execution_admitted: false` explicitly states that inspection did not authorize execution.

`tests/config_cli.rs` covers all scopes and defaults, byte-preserving repeat bootstrap, malformed grammar/help, trusted three-scope inspection, missing/project-local trust inputs, deterministic redaction, HEAD-vs-dirty behavior, reduced permissions, path traversal/redirect refusal, and the legacy positional init path.
