# Pinned run configuration

New `storyflow init ID [BASE]` runs resolve configuration once, save a schema-1 `config-snapshot.json` inside `.storyflow/ID/`, and bind its SHA-256 into the init journal event. The snapshot includes the story ID, exact approved baseline, effective policy, model hints, capability decisions, provenance and complete selected agent contracts with exact-byte digests. Typed serialization uses ordered maps and declaration-ordered lists; whitespace in the stored JSON is not part of the digest.

Every subsequent command replays the journal and verifies the snapshot binding, schema, story/baseline identity, mandatory policy floor and agent digests before returning a packet or changing state. `next` dispatches saved contract bytes. `gate` runs the saved checks in first-declaration order after scope/protection validation against the pinned baseline and candidate. Editing or deleting external source files, changing host context, or modifying worktree configuration does not change an active run. Resume never resolves those files again.

## Compatibility and explicit scope admission

The positional CLI grammar and revision sequence remain unchanged. With no `STORYFLOW_HOST_CONTEXT`, new runs resolve only the approved repository tree, accepting either legacy v1 normalization or explicit v2 repository configuration. No ambient home directories or workspace markers are consulted. V1 still selects its four pinned legacy contracts; v2 requires planner, implementer and reviewer. CLI workers retain the 1–4096 nonblank UTF-8 byte limit.

To enable User → registered Workspace → Repo resolution, the invoking host explicitly sets `STORYFLOW_HOST_CONTEXT` to a native absolute JSON file path and `STORYFLOW_HOST_CONTEXT_ROOT` to a separately authorized native absolute directory before **initialization**. The file must be beneath that directory. Both inputs must come from the invoking user outside project-controlled instructions; the manifest cannot authorize its own location. These are host admission inputs, not configuration scopes or bootstrap commands. Review the authorized scopes and their executable checks before enabling them. Missing authorization, relative paths and invalid, unavailable or untrusted inputs fail without repository-only fallback.

The authorized directory and actual opened file must be outside every Git checkout, including other repositories and worktrees with a `.git` file. Secure component traversal enforces containment and validates the opened target's checkout ancestry before reading or parsing its bytes. Linux rejects escapes and validates in-root link targets; Windows rejects reparse points, including junctions. The same opened handle remains live through validation and the bounded 64 KiB read, so replacing a pathname cannot substitute different bytes. Authorization of directory ownership/ACLs remains the invoking host's responsibility. Native lookup paths must have matching directory spelling; canonical identities inside the manifest use the rules below.

For example, a host may authorize `C:/Users/example/AppData/Roaming/StoryflowHost` using `STORYFLOW_HOST_CONTEXT_ROOT` and select its `host.json` using `STORYFLOW_HOST_CONTEXT`. A repository-local `./host.json`, an absolute file inside a checkout, or a link into one is refused before configuration resolution or snapshot publication.

```json
{
  "version": 1,
  "user_base": "C:/Users/example/AppData/Roaming",
  "trusted_user_base": "\\\\?\\C:\\Users\\example\\AppData\\Roaming",
  "user_directory": "Storyflow",
  "trusted_workspace_roots": ["\\\\?\\D:\\Product"],
  "workspace_selection": null
}
```

`user_base` is a native absolute lookup path, while `trusted_user_base` and each `trusted_workspace_roots` entry are exact canonical identities from `std::fs::canonicalize` (including the Windows verbatim prefix). Canonicalization does not grant authorization: the invoking user must authorize those identities. `user_directory` is relative to that base. The optional `workspace_selection` is a native absolute registered root; it cannot override ambiguous membership. On Linux a typical base is `/home/example/.config`, directory `storyflow`, and canonical identities have ordinary absolute path syntax. Existing secure-reader platform limits still apply; see [the resolver documentation](config-resolver.md).

The context file accepts only the fields above, is limited to 64 KiB, and contains no credentials. Registration remains in the User config; a registration cannot authorize a root. The library APIs `config::resolve_repository` and `snapshot::RunSnapshot::resolve` expose repository-only and explicitly scoped snapshot construction for other hosts, respectively. Constructing a snapshot does not authorize worker execution.

## Policy enforcement and its boundary

Protected paths and checks accumulate; an empty narrower list removes nothing. Conflicting check definitions and attempted capability re-grants fail resolution with both origins. Contract overrides select complete artifacts and cannot modify these rules. There is no CLI operation to skip a mandatory check, and review remains reachable only after every pinned check passes.

For snapshot-backed runs, gate validates both baseline and candidate Git path namespaces before creating a checkout or starting a check. Unsafe portable path components, short aliases and case/canonical-Unicode collisions across either tree are rejected. Directory prefixes participate in collision detection, including aliases against unchanged baseline paths. Protected matching uses slash boundaries and conservative Unicode case/canonical normalization; it applies to newly created descendants without requiring the prefix to exist. A sibling such as `future/outputs` does not match `future/output`. Existing pre-snapshot runs retain their original matching behavior.

CLI repository discovery, commit lookup, pinning and gate Git operations clear ambient `GIT_*` redirects and disable replacement objects so changing the caller's object/index/worktree environment cannot substitute another repository for the approved inputs.

The current manual CLI host has an unrestricted `read`, `write`, `git`, `shell` worker profile. It cannot enforce reduced worker permissions, so it refuses to initialize or dispatch a snapshot whose effective capabilities differ from that profile. Unknown capabilities fail resolution. This admission check does **not** implement a managed tool runtime, sandbox, provider adapter or process-tree timeout handling; those remain issue #9 and later host work. Hosts must enforce their own worker boundaries. Git gates continue to execute approved project commands with the caller's permissions.

## Audit, recovery and rebind

`status` adds `config_mode` (`snapshot-v1` or `legacy-baseline`) and `config_sha256`. `audit.configuration` includes the snapshot digest, scope/provenance metadata, agent versions/digests, check IDs/origins, protected paths and capability decisions. It excludes contract content, command programs/arguments and model values. Packets include this metadata and the selected worker contract. Secrets must remain outside configuration, contracts, reports and gate logs; the local snapshot necessarily stores complete execution inputs, not a secret-redaction substitute.

Existing journals whose init event has no config digest replay unchanged against their Git baseline. They never discover external configuration or create a snapshot automatically. An absent optional digest is omitted during serialization so historical journal hashes remain valid.

Snapshot publication precedes init-event publication, under the existing run lock, using a synced temporary file and rename. If initialization stops before the event is published, retry `init`: an empty run can rebuild its orphaned snapshot. Once the init event exists, missing, modified, invalid or unsupported snapshots fail closed on **all** commands. Restore the original snapshot together with the existing journal and Git objects; keep it when moving a run. Pending writes are not authoritative. This uses the journal's existing filesystem durability model, not a guarantee against storage failure or a writer who can rewrite the complete journal and snapshot together.

Active-run migration/rebind is unsupported, including migration of legacy runs. `init` refuses an existing run; changing the environment cannot rebind it. To adopt changed policy, finish or abandon the current run and initialize a new story ID or dedicated worktree from a separately approved baseline. No command silently rewrites active state.

## Validation

`tests/run_config.rs` exercises real CLI subprocesses and isolated repositories: legacy lifecycle/revisions, three-scope replay after parent changes/disappearance, overridden Markdown asking to skip mandatory checks, conflicting checks, tool expansion and unsupported restrictions, snapshot corruption/loss across reads and mutations, deterministic snapshot serialization, interrupted initialization, invalid host-input refusal, protected descendants/siblings, portable/case/Unicode aliases, and pre-snapshot journal replay. The gate fixture is a filtered invocation of the test binary, so these Rust tests do not require PowerShell or a provider.

Also run `cargo test --locked --all-targets`, formatting, Clippy, the schema fixture validator, slug demo and CLI matrix. These complement existing resolver trust/Git/object tests; they do not claim enforcement of operations made directly through a host's own tools.
