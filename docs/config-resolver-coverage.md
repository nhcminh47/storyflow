# Issue #4 semantic fixture coverage

Source: the 30 `resolver-pending` scenarios in [cases.json](config-fixtures/cases.json), unchanged from PR #16. `resolver::semantic_fixture_inventory` in [tests/config.rs](../tests/config.rs) executes **26** outcomes and explicitly counts **4 deferred** outcomes. The fixture names below are exact lookup keys. An accepted fixture gets neutral pinned contracts only for workflow roles not declared anywhere in that fixture. No fixture expectation is changed.

| Scenario | Classification | Executable coverage / disposition |
| --- | --- | --- |
| duplicate raw key | Parser validation | Inventory; `parser_rejects_duplicates_nulls_types_and_versions` also covers nested objects/arrays |
| legacy standalone | Resolver validation | Inventory; `legacy_normalization_and_implicit_agent_conflict` |
| legacy inherits mandatory check | Policy composition | Inventory; ordered inherited + legacy checks |
| legacy conflicts with mandatory check | Policy composition | Inventory; case-insensitive conflict and both origins |
| missing repo configuration | Git-tree artifact resolution | Inventory; working-tree fallback prohibited |
| trusted registered workspace | Filesystem discovery/trust | Inventory; unique explicit member identity |
| registered but untrusted workspace | Filesystem discovery/trust | Inventory; fails before marker read |
| unregistered workspace marker | Filesystem discovery/trust | Inventory; malformed unregistered marker deliberately ignored |
| ambiguous workspace | Filesystem discovery/trust | Inventory; `ambiguous_membership_ignores_explicit_selection` |
| workspace agent symlink escape | Filesystem discovery/trust | Inventory; real Windows junction / Linux symlink to outside root |
| identical inherited agent | Resolver validation | Inventory; preserves User provenance |
| explicit workspace override | Resolver validation | Inventory; selects Workspace bytes/version |
| explicit repo override | Resolver validation | Inventory; selects nearest valid Repo override |
| agent conflict | Resolver validation | Inventory; differing inherited contract without true override |
| duplicate agent IDs | Parser validation | Inventory; duplicate declarations fail even when identical |
| missing inherited target | Resolver validation | Inventory; true override requires inherited ID |
| override cannot regrant capability | Policy composition | Inventory; `regrant_conflict_points_to_the_actual_removal` |
| override cannot skip mandatory gate | Future runtime enforcement | **Deferred #5/#9**: attempted skip must be rejected by integrated enforcement. Resolver retains mandatory checks regardless of Markdown; composition tests cover that prerequisite. |
| pinned baseline survives worktree edits | Git-tree artifact resolution | Inventory; altered working config and deleted working contract |
| pinned agent symlink | Git-tree artifact resolution | Inventory; real 120000 index/tree entry |
| pinned agent gitlink | Git-tree artifact resolution | Inventory; real 160000 commit entry |
| pinned agent directory | Git-tree artifact resolution | Inventory; real tree at artifact leaf |
| missing pinned agent | Git-tree artifact resolution | Inventory; missing blob rejected |
| invalid Git path ../planner.md | Parser validation | Inventory; rejected before Git lookup |
| invalid Git path agents/./planner.md | Parser validation | Inventory; rejected before Git lookup |
| invalid Git path agents/NUL.md | Parser validation | Inventory; platform-independent device-name rule |
| invalid Git path agents/planner.md:stream.md | Parser validation | Inventory; stream syntax rejected |
| future protected descendant | Future runtime enforcement | **Deferred #5**: actual candidate change must fail integrated protection checks. `lexical_paths_and_protection_matching` covers nonexistent-prefix matching; composition retains the declaration. |
| protected sibling is outside prefix | Future runtime enforcement | **Deferred #5**: candidate scope/protection result belongs to the integrated gate. Boundary-only matching is separately tested. |
| platform alias in candidate | Future runtime enforcement | **Deferred #5/#9**: candidate namespace/alias collision validation belongs to enforcement. Resolver input alias validation is separately tested. |

Additional regressions cover OS environment lookup, absent optional directory/file vs unavailable/untrusted base, invalid registered markers/members, duplicate roots/members, trust for all roots before any marker, selection errors, model tiers and per-field origins, duplicate raw JSON keys, nulls/types/versions/unknown fields, check definitions, built-in floors, capability ceilings/deny precedence/unknown host IDs, exact byte digests, deterministic three-scope serialization, missing checks/workers, legacy implicit-agent conflicts, replacement objects, Git case spelling, intermediate non-trees, invalid UTF-8, and Windows open-handle replacement attempts.

Run snapshot persistence, resume verification, rebind and integration are entirely deferred to #5. Actual tool enforcement/isolation is deferred to #9. These are not reported as implemented or passing. No new scope bootstrap/inspect/validate commands (#6) are included.
