# Internal configuration resolver

`storyflow::config::resolve(&ResolveInput) -> ConfigResult<EffectiveConfig>` is an independently usable library API. New story runs call the resolver and pin its output; see [run snapshots](run-snapshots.md) for CLI admission, replay, enforcement and compatibility. `resolve_repository` explicitly resolves only the approved repository tree without discovering external scopes. The [scoped document contract](config-scopes.md) and schema remain the source of validation rules.

## Inputs and output

The host supplies an exact approved `GitBaseline { root, commit }`, a `UserLocation`, optional workspace selection, `HostTrust`, and supported/granted capability sets. Approval of the commit and ownership/control of the user base are host responsibilities. `HostTrust` contains **canonical** identities produced by the filesystem, including the Windows verbatim prefix returned by `std::fs::canonicalize`. Trust must come from the invoking user outside all scope documents. Registration never creates trust.

`UserLocation::from_environment(UserPlatform, &UserEnvironment)` implements APPDATA / absolute XDG_CONFIG_HOME / HOME lookup using an explicit snapshot of the invoking user's environment. It never consults a home directory implicitly. The base must already exist and be host-authorized. An absent Storyflow directory or optional config file is allowed; unavailable/untrusted bases and invalid existing files are errors. Explicit `UserLocation` values also support test filesystems without accessing real home configuration.

The result contains ordered scope identities, model tiers, selected complete contracts and exact UTF-8 content, SHA-256 digests, mandatory checks, protected paths, built-in floors and effective capabilities. `Sourced<T>` and `Origin` retain the declaring scope, document source and field path. Capability provenance includes the exact removal field for every supported capability; the ordered constraints retain allow/deny declarations. Diagnostics return a stable code, source, field, safe constant explanation and a second origin for conflicts. They omit command arguments, configuration values, artifact content and Git stderr.

```rust,ignore
use storyflow::config::{resolve, ResolveInput, EffectiveConfig, ConfigResult};

fn load_for_host(input: &ResolveInput) -> ConfigResult<EffectiveConfig> {
    let effective = resolve(input)?;
    // This is input resolution, not permission to dispatch or execute a run.
    Ok(effective)
}
```

`parse_document(bytes, expected_scope, source)` provides isolated parser validation. Public document structs are data models; constructing or deserializing a struct directly is not a substitute for this validated entry point. `GitBaseline::read` separately exercises authoritative artifact lookup. Filesystem traversal and discovery are separate modules with temporary-directory tests. No resolver operation executes a declared check or interprets Markdown as policy.

## Determinism and compatibility

Composition uses User → Workspace → Repo and ordered vectors for checks/constraints/scopes, with BTree maps/sets for indexed values. Check IDs use ASCII lowercase; identical definitions retain their first position and provenance. Protected matching retains v0.1 Unicode lowercase and slash boundaries without requiring targets to exist. A child's empty lists cannot remove inherited checks or protections. Agent deduplication compares versions and exact consumed bytes, including line endings. True overrides select the whole artifact and change provenance even when identical.

V1 normalization retains legacy path validation, check constraints/order, protection floors, the 16 KiB policy limit and four pinned contracts with their existing nonblank 4 KiB limit. Artifact versions become `legacy-v1` in memory. User planner version `1` plus implicit repository planner version `legacy-v1` is an `agent_conflict`, even for identical bytes. Resolve it by explicitly migrating the repository document to v2 and declaring the intended agent with `override: true`; the resolver never rewrites input.

The binary shares its `Check` shape, protection constants and legacy role list with the library. New initialization saves the effective configuration and agent bytes with a journal-bound digest; packets and gates consume those pinned inputs. Pre-snapshot journals keep their original baseline behavior. Required effective workers are planner, implementer and reviewer, derived from the existing context dispatcher. Legacy v1 initialization also pins the orchestrator.

## Filesystem and Git security

Every registered root is opened, canonicalized and authorized before **any** workspace marker is read. Every registered marker and member is validated, even when it does not claim the invoking repo. Only the unique matching workspace's artifacts are opened. Unregistered markers are never inspected. Member identities must be exact canonical Git top-levels. An explicit selection cannot resolve ambiguous claims.

Windows traversal opens each component with `OPEN_REPARSE_POINT`, retains ancestor handles, denies write/delete sharing and checks metadata and the final opened name. This blocks component replacement, rename and reparse mutation while consuming bytes. Short-name aliases fail lexical or opened-name validation. See the [CreateFile sharing and reparse semantics](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew). The handle regression attempts directory rename, file rename and overwrite while the read handles are live, then confirms rename succeeds after handles close.

Linux traversal uses `openat2` relative to an owned directory descriptor with `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`, validates the opened object's type and verifies its procfs identity before reading. User config reads additionally reject checkout ancestry of the **actual opened target** before consuming bytes, including links into nested in-root checkouts. The same handle remains live across validation and reading; replacing the pathname cannot redirect consumption. Nonblocking open prevents a FIFO from hanging before type validation. The kernel constrains the lookup itself; canonicalize-then-open is never used. See [openat2](https://man7.org/linux/man-pages/man2/openat2.2.html).

An `ENOENT` from a link-following lookup does not prove that an optional entry is absent. Linux probes the requested entries with descriptor-relative `O_PATH | O_NOFOLLOW` opens, and separately validates any link targets with the same beneath/magic-link restrictions. Existing dangling symlinks return `dangling_symlink`; uncertain concurrent replacement fails closed. Only a genuinely absent entry permits omission. This also applies to the optional Storyflow directory and intermediate entries in an explicit location.

Platform limits fail closed:

- Windows external reparse points (including in-root links/junctions and cloud placeholders) are rejected as `path_escape`, with an explanation to use physical in-root paths. No target bytes are read. Windows filesystems must honor sharing and opened-name queries; failures return `unsafe_filesystem`.
- Linux requires kernel 5.6+ `openat2` and procfs. Absolute symlink targets, magic links and any traversal outside the trusted root are rejected. Relative in-root links are supported. Unsupported kernel/filesystem operations return a precise diagnostic.
- macOS and other platforms return `unsupported_secure_filesystem` for external traversal. Their parser, Git-tree reader and environment-location logic remain independently usable. A secure platform backend is required before full resolution is usable there.
- Host authorization attests user control of the configuration base; the resolver does not manage ACLs, interactive trust or a persistent trust database. This API must not be used with trust identities supplied by project documents.

Git reads use an exact commit ID, disable replacement objects for **all** authoritative commands, and clear Git environment variables that can redirect object/repository lookup. Each path component is matched by exact UTF-8 spelling in a tree listing; intermediate entries must be trees and leaves regular blobs, with actual object types checked. Config and contract bytes always come from the approved object database. Worktree edits/deletions have no effect. No Git stderr is copied into diagnostics.

## Regression inventory and validation

See [the 30-scenario coverage matrix](config-resolver-coverage.md). The schema suite retains its original meaning: semantic outcomes remain labeled pending in the specification validator. Rust executes 26 resolver/parser/discovery/Git/policy outcomes; four actual runtime outcomes remain deferred, not counted as passes. Fixture setup supplies neutral pinned contracts for unrelated required roles omitted by the specification fixtures.

All tests use temporary directories and Git repositories without providers, credentials, real user config or network calls. Windows junction creation can require running tests outside a restricted sandbox; this is test setup, not a resolver permission bypass.

Validation on Rust 1.99.1 and PowerShell 7.6.9:

| Command | Actual result |
| --- | --- |
| `cargo fmt --check` | Passed |
| `cargo test --locked` (Windows GNU) | 26 passed: 1 handle unit test + 25 integration tests; 0 failed/ignored |
| `cargo test --locked` (WSL Ubuntu Linux) | 30 passed: 1 handle unit test + 29 integration tests; 0 failed/ignored |
| `cargo clippy --locked --all-targets -- -D warnings` | Passed on Windows GNU |
| `pwsh -NoProfile -File docs/config-fixtures/validate.ps1` | 3 schema accepts, 12 rejects, 1 parser rejection; 46 v2 documents shape-checked; 30 semantic outcomes still labeled pending by this unchanged script |
| `pwsh -NoProfile -File examples/slug/demo.ps1` | Done, revision 8, 9 journal events, expected initial red gate followed by scope + acceptance passing |
| `pwsh -NoProfile -File examples/matrix/run.ps1` | All 7 cases passed |
| `cargo test --locked --offline --manifest-path examples/slug/gate/Cargo.toml --target-dir target` | Intentionally broken baseline: 1 passed, 2 expected failures (also established before changes) |
| `git diff --check` | Passed |

The matrix initially failed with Git `'$GIT_DIR' too big` under the sandbox's long temp prefix. Both scripts passed with TEMP/TMP set to the shorter ignored `target/tmp` directory; their source was unchanged. The active GNU toolchain initially lacked formatter/Clippy components; installing those components made the exact requested commands available. The installed MSVC formatter worked, but MSVC Clippy could not link without `link.exe`; final validation uses GNU. Cargo also emitted sandbox incremental-cache hard-link fallback warnings; Clippy reported no code warnings. Linux Rust/linker setup was isolated in ignored `target` subdirectories because Ubuntu initially had neither Rust nor `cc`; tests themselves do not download tools or dependencies.

The Issue #4 source review checked contract/schema preservation, trust-before-read ordering, opened-object containment, replacement-object isolation, exact byte provenance, capability removal origins and CLI compatibility. The validation results above describe that resolver baseline; Issue #5 integration and its separate regression suite are documented in [run snapshots](run-snapshots.md). Independent PR review remains required.
