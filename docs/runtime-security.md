# Runtime security

Implementation baseline: `54aceb7b841175b93d6b73de9429ee535485001e` (merged PR #24).
Issue #9 is independent of worker commit integration (#10).

## Implementation contract

* A managed-only runtime authenticates the existing story snapshot and current
  scheduler lease under the existing story-first/task-second locks. Each request
  authorizes its capability and task path before file or command execution.
* Pinned capability grants are ceilings. Missing grants deny access. Arbitrary
  shell and mutating Git requests remain unsupported even when granted; no
  arbitrary executable is available in the initial managed-only backend.
* Confined regular-file access rejects traversal, portable aliases, protected
  descendants, links, nonregular files and nested repository boundaries.
* Trusted gates retain pinned candidate/scope checks and journal results, with
  environment filtering, bounded output and process-tree cleanup.
* Requested command sandboxing fails closed. A worktree is an input checkout,
  never a sandbox. External agent tools are outside this managed boundary.

## Test matrix

| Gate | Evidence required |
| --- | --- |
| G1 | Denied read/write/Git/shell; no side effects; snapshot tampering; no ambient escalation |
| G2 | Traversal, symlink/hardlink, aliases, protected descendants, file/submodule boundaries |
| G3 | Spawned child terminated on timeout and after direct parent exit |
| G4 | Environment sentinel absent; output bounded; unsupported sandbox refuses execution |
| G5 | Existing init/next/submit/gate/audit tests and CLI scenarios |
| G6 | Format, Clippy, all target tests, build; Windows locally, Linux in CI |

The following sections record the final API, guarantees and residual risks.

## Managed execution boundary

`storyflow init ID [BASE] --managed-runtime` explicitly selects the initial
managed-only host profile. `managed_only: true` is included in the digest-bound
snapshot. Older snapshots omit this optional false field, preserving their
canonical bytes and digests. Existing manual runs still refuse reduced worker
profiles; they cannot be rebound. Config schema, story events, task events,
scheduler state, attempts and lease formats are unchanged.

The host passes one schema-1 JSON request on stdin to `storyflow-runtime ID TASK`:

```json
{
  "schema": 1,
  "lease": { "attempt_id": "from-dispatch", "lease_id": "from-dispatch", "owner": "from-dispatch" },
  "operation": { "op": "read", "path": "app/value.txt" }
}
```

The lease must come from `storyflow-tasks dispatch`; these example identities
are placeholders. Requests cannot supply grants, a root, scopes or configuration.
Input is bounded to 2 MiB and read before acquiring locks. The runtime retains the
verified story lock and task lock through execution, requires a current running
attempt with the exact unexpired token, graph/story/task revisions and owner,
and checks expiry again immediately before authorization and after durable audit I/O. Lease identifiers fence
attempts; they are not authentication credentials. The invoking OS user and host
must authorize access to this CLI and its run directory.

| Operation | Required pinned grants | Additional restriction |
| --- | --- | --- |
| `read`, with `path` | `read` | Exact task read scope; bounded UTF-8 regular file |
| `write`, with `path`, `content` | `write` | Task write scope intersected with pinned story scope; at most 1 MiB |
| `git_read`, with `path` | `git` and `read` | Regular UTF-8 blob from pinned baseline, task read scope, at most 1 MiB |
| `git`, with `args` | `git` | Always refused: arbitrary Git and mutations unsupported |
| `shell`, with `program`, `args` | `shell` | Always refused: arbitrary executables unsupported |

Managed grants require an explicit `tools.allow` in pinned configuration; absence
defaults to no grants. This tightening is saved with host provenance in the
snapshot; inherited denials cannot be regranted. Missing grants return `capability_denied`; unsupported commands return
`unsupported_managed_operation`. No command starts for either rejection. Workers
cannot add a grant with a request, mutable config, ambient environment or agent
Markdown. Existing Rust candidate/gate checks remain necessary after managed
writes; a successful managed request cannot advance a story or approve a commit.

All paths use portable repository-relative components and NFC spelling. Matching
uses exact task-scope spelling and slash boundaries, with conservative case and
canonical-Unicode normalization for protections and alias detection. Protected
paths are refused for reads as well as writes. A broad scope never authorizes its
protected descendants. Baseline Gitlinks/symlinks and all descendants of baseline
leaf files are refused; replacing a file with a directory cannot expand authority.
Filesystem traversal also refuses nested repository markers, symbolic links,
Windows reparse points, hardlinks and nonregular files. Parents must already
exist; the initial backend does not create directories or delete files.

Linux traversal uses directory descriptors, `openat`/`O_NOFOLLOW`, and an `O_PATH`
inspection before opening the validated regular file via trusted procfs. New files
use exclusive creation. Windows holds ancestor handles without delete sharing,
opens with `FILE_FLAG_OPEN_REPARSE_POINT`, and validates the final file handle
before truncating. These mechanisms resist pathname substitution within their
stated host boundary. The root must be a canonical host-authorized directory;
the host owns its ancestors, run metadata and ACLs and excludes concurrent hostile
same-identity processes. This is not a privilege boundary against a user who can
already rewrite Git objects, the full journal, protected directories or open
files outside this API.

Every admitted operation first syncs a bounded intent/denial record in
`.storyflow/ID/security-events.jsonl`, then records completion/failure. Records bind
snapshot digest, story/task/attempt and graph revision, operation kind, phase and
constant error code. They contain no requested path, content, command arguments,
owner, environment values or token. The 4 MiB cap reserves space for terminal
evidence and fails closed before another effect. A crash may leave an intent
without completion; writes are not transactionally coupled to audit publication.
This sidecar does not alter the existing story/task journals or claim protection
against malicious whole-history replacement. Admission failures before a verified
run exists are returned as sanitized JSON rather than written into untrusted state.

`storyflow::security::ManagedRuntime` is a trusted-host integration API: callers
must obtain its context, task and attempt identity from verified journals while
retaining their locks. Supplied leaf boundaries are optional additional host floors;
Rust always verifies the requested pinned path itself after authorization. Constructing these structs yourself
confers no authority. `security::process` supervises trusted infrastructure commands;
it is not a worker tool interface. `GitBaseline::read_bounded` is now public for
bounded pinned-blob reads. Targeted metadata uses `GitBaseline::lookup_managed_blob`
and `read_blob_bounded`. No Issue #10 contract or scheduler API changes are needed.

## Gate execution and sandbox selection

The unchanged scope gate validates the pinned candidate tree before checkout and
before command execution. Commands use explicit programs and argument arrays;
Windows `.bat`/`.cmd` targets are refused because they would introduce implicit
shell interpretation. Existing checks that explicitly invoke an interpreter remain
trusted commands. Candidate identity, check outcomes, timeout blocking, revisions,
log references and audit semantics remain intact. A timeout cannot pass.

The process runner clears the environment, retaining only `PATH`, `TEMP`, `TMP`, `TMPDIR` and, on Windows,
`SystemRoot`, `WINDIR`, `ComSpec`, `PATHEXT`, `SystemDrive`, `ProgramData`,
`ALLUSERSPROFILE`, `USERPROFILE`, `LOCALAPPDATA`, `APPDATA` and the explicit MSVC location profile
(`ProgramFiles`, `ProgramFiles(x86)`, `ProgramW6432`, `VSINSTALLDIR`, `VCINSTALLDIR`,
`VCToolsInstallDir`, `VCToolsVersion`, `WindowsSdkDir`, `WindowsSDKVersion`,
`LIB`, `LIBPATH`, `INCLUDE`, `VSCMD_ARG_TGT_ARCH`, `VSCMD_ARG_VCVARS_SPECTRE`).
It sets `TZ=UTC`, `LC_ALL=C`. PATH and
the OS/toolchain installation are trusted host inputs. HOME, API keys, Git redirects,
loader hooks, language startup hooks and unlisted caller tool settings are not inherited.
Git authority reads additionally disable global/system configuration, replacement
objects and implicit lazy fetching; Git must support `--no-lazy-fetch` (older
versions fail closed). Bare executables resolve only through absolute trusted PATH
entries, preventing current-directory tool shadowing. Authoritative Git also refuses
executables inside any Git checkout, including symlinked/absolute PATH entries; gate checkout maintenance disables hooks and fsmonitor. Gates receive no
explicit credentials. Tools that need dependencies should use a preprovisioned
trusted toolchain/cache or fixture; credential-dependent online checks may now fail.

Each stream retains at most 1 MiB while continuing bounded nonblocking drains.
Each lifecycle cleanup pass has a 2-second deadline (at most one guard retry); checkout Git maintenance has a 60-second
deadline and bounded output. Cleanup failure becomes an error, not a passed gate.
Checkout evidence may remain for inspection after an operational failure. Processes
are cleaned both on timeout and ordinary direct-parent exit; uncollected output or
process cleanup failure returns an error carrying bounded partial capture. Gates
persist that capture in their existing stdout/stderr logs and retain a non-success
outcome; pre-launch failures have empty capture. `process::run` returns
`Result<ProcessResult, ProcessError>`; `.kind()` exposes the I/O classification
without putting command arguments or captured data in the error message. The OS may retain dead zombie records
until their parent reaps them; those records cannot execute.

| Platform | Trusted gate supervision | Managed-only filesystem | Hostile command sandbox |
| --- | --- | --- | --- |
| Linux | Dedicated process group; kill group and confirm no live members; bounded reaping/draining | Descriptor-relative access; trusted mounted `/proc` required | Unsupported; explicit requests fail closed |
| Windows | Suspended creation; assign kill-on-close Job Object before resume; terminate/wait whole job | Pinned ancestor handles; all reparse points refused | Unsupported; Job Objects supervise processes, not file/network access |
| Other OS | Refused before process spawn | Refused | Refused |

Linux commands that deliberately leave their process group/session can escape this
trusted supervisor. Neither platform's trusted mode restricts filesystem, network,
IPC, credentials acquired by tools themselves or access to user configuration on
disk. Windows Job Objects do not provide those restrictions either. Run untrusted
project commands in a separately provisioned VM/container with host-enforced limits.

`storyflow gate ID REV --sandbox managed-only` requests the narrow backend. After
scope validation it records `sandbox_command_unsupported` and blocks without
starting any command or checkout. Any other requested sandbox name is rejected.
There is no fallback to trusted execution. Default `storyflow gate ID REV` retains
the reviewed trusted command workflow. The managed-only backend enforces file/blob
operations, not arbitrary sandboxed programs; it is intentionally narrow.

External Codex/Claude/Cursor tool calls and workers that use their own OS tools are
outside this boundary. The host must actually route requests through the managed
runtime. Setting a mode or writing Markdown alone cannot restrict another program.

## Ownership and validation

Changes are confined to security modules, the new runtime companion, security
integration in main/run configuration/snapshot, shared Git safety helpers, gate
execution, new security/gate tests, this document and security README updates.
No integration worktree, Issue #10 files, scheduler state machine, attempt identity
or journal event protocol is modified. The invoking user approved the two narrow
ownership exceptions below: fixture argument transport and task-lock resource cleanup.

Validation evidence is recorded in the PR. Local platform: Windows GNU, Rust
1.99.1. Ubuntu WSL is installed without Rust; GitHub CI must pass Linux and Windows
before this change is considered ready. The tests use real CLI requests and native
subprocess trees, including grandchildren, and exercise denied operations with
sentinels rather than treating successful authorization as proof of confinement.

## Contract Change Request CCR-9-1 (approved and implemented)

Two existing `tests/run_config.rs` tests observe gate execution through
`STORYFLOW_TEST_GATE_MARKER`. The required environment filter intentionally does
not pass that arbitrary caller variable, so their marker assertions fail even
though all pinned gates pass. Do not weaken the production allowlist for a test.

The invoking user approved updating only that existing fixture so `check` takes
its marker path, supplies it as an explicit pinned `--skip` argument, and
`gate_helper` reads the argument instead of the environment; remove the unused
fixture environment injection. The five `check` call sites pass
`&f.root.join("gate-marker")`. No assertions, scheduler protocol or production
behavior change. The exception changes fixture transport only; the existing assertions and production
environment policy are preserved. The separate task-lock exception is described below.

Primary API references: [Windows CreateFileW](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew),
[Linux openat](https://man7.org/linux/man-pages/man2/openat.2.html),
[Git environment and no-lazy-fetch](https://git-scm.com/docs/git).
Windows MSVC discovery is validated by the CI slug demo; retaining Program Files
and Windows profile/data locations is required for installer discovery. A platform test
compares filtered vswhere discovery against the invoking host. References:
[Microsoft installer documentation](https://github.com/microsoft/vswhere/wiki/Installing)
and [Rust MSVC discovery source](https://github.com/rust-lang/cc-rs/blob/main/find-msvc-tools/src/find_tools.rs).

## Contract Change Request CCR-9-2 (approved and implemented)

Initial Ubuntu CI runs encountered `task_locked` after dropping and immediately
reopening the baseline TaskJournal, in separate existing DAG/scheduler tests.
Rust's File lock remains held by inherited descriptors until every copy closes;
an unrelated parallel fork can temporarily retain it before exec. A standalone
WSL fork/flock experiment reproduced this behavior. A later green run did not
resolve the defect; the original failed CI evidence is retained in the PR.

The invoking user approved the narrow `src/tasks/journal.rs` ownership exception.
`TaskJournal::drop` now explicitly releases the lock in its acquiring process.
A private, in-memory owner PID prevents a forked copy from explicitly unlocking
its parent's lock. Constructing this owner before fallible directory setup also
covers failed initialization/replay paths. No journal operation remains in flight
at safe Rust destruction. No state, event, attempt identity, schema or public API
changes are introduced.

`tests/security_lock_inheritance.rs` holds the inherited descriptor open in a
Linux child and verifies that dropping the parent journal permits immediate
reopening before the child is released. A native close-only File control must
remain locked in the same fork, proving the inherited descriptor is still live.
The child uses only async-signal-safe
libc operations and is reaped before asserting the result. Existing lock exclusion,
replay, recovery and journal-byte tests remain unchanged. See the
[Rust File lock contract](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock).

## PR review hardening

Managed requests no longer recursively enumerate the baseline under story/task
locks. Missing grants, unsupported commands and policy scope/protection failures
are rejected before any namespace lookup. Authorized file/blob operations walk
only immediate sibling trees along the requested path, check portable aliases in
each visited directory, and reject symlink, Gitlink and regular-file descendants.
A missing baseline component permits a new scoped filesystem path; the confined
filesystem backend still checks its actual components. Story candidate gates keep
their full pinned-tree validation unchanged.

The lookup accepts at most 4 KiB / 64 path components, 1 MiB each for an individual
raw tree and its listing, 16,384 siblings per tree and 4 MiB combined raw/listing
metadata per operation. Raw object size is checked before listing; oversized or
ambiguous namespaces fail closed with a sanitized `namespace_limit` / `path_denied`
error. There is no mutable manifest cache or unrestricted fallback. The lease is
checked again after metadata lookup and before the file/blob effect. Tests use
250,000 logical files in shared Git trees, oversized requested trees, denied
requests in an invalid namespace, and pinned sibling aliases.

Linux per-process `/proc/PID/stat` reads treat only `ENOENT` and `ESRCH` as a
vanished process. Other errors still fail cleanup. Tests explicitly reject
permission and I/O errors, so an unrelated process exit cannot disable cleanup
verification or conceal inaccessible procfs.

Post-spawn process errors preserve at most 1 MiB per captured stream in
`ProcessError`; success is never inferred from a failed lifecycle. Private test
seams inject errors after real bounded execution and verify persisted gate logs,
non-success outcomes, truncation and exclusion of command/argument/cause secrets
from the gate's error detail. No worker-controlled failure injection was added.