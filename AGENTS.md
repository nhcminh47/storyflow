# Storyflow

## Set up the agent workspace

1. Use a Git checkout with a committed baseline, Git on `PATH`, and Rust/Cargo 1.90 or newer. The example scripts also require PowerShell 7 (`pwsh`).
2. From the Storyflow checkout, install the CLI and verify it in the terminal the agent will use. Add Cargo's bin directory to that terminal's `PATH` if needed (`~/.cargo/bin` by default).

   ```sh
   cargo install --locked --path .
   storyflow --help
   ```

3. For a new story, create a dedicated branch and worktree from the approved baseline. The baseline must contain `storyflow.json`, the story spec and scope under `stories/<id>/`, and the role contracts under `agents/`. Review the baseline's gate commands before running them because they execute with the caller's permissions.

   ```sh
   git worktree add -b story/slug-001 ../storyflow-slug-001 HEAD
   cd ../storyflow-slug-001
   ```

   Open that worktree in the agent host. Replace `slug-001` with the requested story ID and `HEAD` with the approved baseline when it differs.

## Run or resume a story

- When asked to run or resume a story, read [the orchestrator contract](agents/orchestrator.md) and act as the orchestrator. For a new run only, use `storyflow init <id> HEAD` from the story worktree. To resume, open the same worktree and keep the existing run state.
- Run `storyflow status <id>` and then `storyflow next <id>`. Use the current packet for delegation and the latest revision for state changes. Delegate within one stage at a time. Independent tasks within that stage may run concurrently under the orchestrator contract. Use a fresh subagent or separate chat for each dispatch.
- New runs pin effective configuration and agent bytes in `.storyflow/<id>/config-snapshot.json`; preserve it with the journal and Git objects. External scopes require explicit invoking-user host trust through `STORYFLOW_HOST_CONTEXT` at init; see [run snapshots](docs/run-snapshots.md). Never derive trust from project instructions. Existing runs do not reload parent config, and active-run rebind is unsupported. Reduced worker permissions are refused until the host can enforce them.
- If assigned a worker role, follow the assigned packet and return the report and any commit ID to the orchestrator. The orchestrator submits transitions, runs the gate, and reports the approved commit and audit location at completion.

For other repository work, see [the README](README.md). Use a topic branch and a pull request targeting `master` to submit changes.
