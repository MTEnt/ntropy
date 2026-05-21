# nTropy (auto-os)

nTropy is a local desktop harness for routing prompts to installed AI coding CLIs from a Tauri + Svelte interface. It is built for full-permission local automation: it can open a workspace, store project conversations and tasks, launch model CLIs in that workspace, stream their output back into the UI, and let those CLIs make changes with the permissions of the signed-in operating system user.

This is not a sandboxed security boundary. Treat it like a convenient control panel for powerful local developer tools.

## Current Product Truth

nTropy currently provides:

- A Svelte 5 desktop UI wrapped by Tauri 2.
- A Rust backend that launches provider CLIs as child processes.
- A workspace file browser and Tree-sitter symbol indexer for Rust, Python, JavaScript, and TypeScript files.
- Local SQLite storage for chats, messages, tasks, and skill definitions.
- Prompt routing across Claude, Gemini, Grok, and Codex CLIs.
- Frontend-managed companion subagents that can be triggered manually or by a structured stdout line.
- A lightweight rules panel and warning log for project conventions.
- A task dashboard for manually run or interval-based prompts while the app is open.

nTropy does not currently provide:

- OS-level sandboxing, filesystem containment, network isolation, or rollback.
- A guarantee that launched CLIs cannot read or write outside the selected workspace.
- A hard security policy engine. The rules gate is advisory in the current implementation.
- A durable background service. Scheduled tasks run from the app session.
- A cross-user secret manager or encrypted chat database.
- A guarantee that every listed provider model name is available to the installed CLI or account.

## Permission Model

The app starts model CLIs with broad local permissions. The current launch paths intentionally prioritize automation over approval prompts:

- Claude: `--dangerously-skip-permissions --permission-mode bypassPermissions`
- Codex: `--dangerously-bypass-approvals-and-sandbox --skip-git-repo-check`
- Gemini: `--skip-trust --approval-mode yolo`
- Grok: `--always-approve --permission-mode bypassPermissions`

The backend sets the child process current directory to the active workspace. That is useful for context and relative paths, but it is not an operating-system sandbox. A launched CLI may still be able to read, write, delete, run commands, use the network, or access credentials according to the normal permissions of the user account and the CLI itself.

Use nTropy only with folders and accounts where this level of automation is acceptable.

## Prerequisites

Install the local app dependencies:

```powershell
npm install
```

Install and authenticate any provider CLI you plan to use:

- Claude Code CLI, available as `claude`
- Gemini CLI, available as `gemini`
- Grok CLI, available as `grok`
- Codex CLI, available as `codex`

On Windows, the backend uses shell-free executable launches. It does not route prompt text through `cmd /C`. If a provider fails to launch, confirm the CLI is installed, authenticated, available through its native executable or Node entrypoint, and allowed to run in non-interactive mode.

## Running

Frontend only:

```powershell
npm run dev
```

Tauri desktop app:

```powershell
npm run tauri dev
```

Build frontend assets:

```powershell
npm run build
```

Run Svelte checks:

```powershell
npm run check
```

The Vite dev server is configured for port `1420` with `strictPort: true` because Tauri points at `http://localhost:1420`.

## Model Routing

Every prompt has an active provider selected in the UI. The backend then applies simple routing rules:

- A prompt starting with `@claude`, `@gemini`, `@grok`, or `@codex` routes to that provider.
- Prompts mentioning `research agent` or `researcher` route to the research mapping, defaulting to Grok.
- Prompts mentioning `backend coder agent` or `backend coder` route to the backend mapping, defaulting to Codex.
- Prompts mentioning `frontend coder agent` or `frontend coder` route to the frontend mapping, defaulting to Claude.
- Prompts mentioning `verification agent` or `verifier` route to the verification mapping, defaulting to Gemini.
- Otherwise, the selected UI provider is used.

Provider mappings and specific model selections are passed from the UI into the backend. The UI offers preset model names, but availability still depends on the installed CLI version, account access, and provider-side support.

For Codex child processes, nTropy also passes `--ignore-user-config --disable plugins --disable remote_plugin --disable shell_snapshot`. Auth still comes from the normal Codex login, but spawned runs do not inherit remote MCP/plugin OAuth configuration from the user's global Codex config. That keeps nTropy runs focused on the selected provider/model instead of trying to warm unrelated Codex-owned connectors.

## Subagent Behavior

The companion agents are canonical roles, not arbitrary hidden workers:

- `research`
- `backend`
- `frontend`
- `verification`

The backend instructs the active CLI to delegate by printing a line in this shape:

```text
SPAWN_SUBAGENT:agent=<research|backend|frontend|verification>,task=<specific task>
```

Only the top-level chat orchestrator session is allowed to create these companion runs. Casual chat can be answered directly by the orchestrator. Non-casual work, including builds, code changes, project inspection, debugging, tests, model routing issues, logs, auth failures, and research, is mandatory-delegation work. When the backend sees a `SPAWN_SUBAGENT` line in stdout from the main session, it emits a Tauri event tied to that prompt's run id. The frontend also deterministically launches the relevant canonical agents for non-casual prompts, so delegation does not depend only on the model choosing to print the line.

Worker sessions receive a different harness: they are told to complete their assigned slice and not print `SPAWN_SUBAGENT`. If a worker or scheduled task still prints a spawn line, the backend ignores it and logs that nested delegation is disabled. The frontend also rejects spawn events that are missing a parent session id, have a stale run id, or do not come from the live main session. This keeps one user request from turning into recursive fan-out.

Important limitations:

- Invented agent names are ignored.
- Nested subagent spawning is ignored; the main orchestrator is the only coordinator.
- The frontend must be running to receive the event and launch the companion run.
- Subagents use the same full-permission CLI execution model as normal prompts.
- Delegation can increase token usage and tool activity quickly, so nTropy limits automatic fan-out to one live run per canonical role.

## Project Storage

When a project is opened or created, nTropy ensures the project has a `.agents` directory and opens:

```text
.agents/nentropy.db
```

SQLite WAL mode also creates sidecar files such as:

```text
.agents/nentropy.db-shm
.agents/nentropy.db-wal
```

The database stores:

- Chat sessions and messages.
- Task definitions and status.
- Skill definitions and trigger tags.
- Migration history.

The database is local runtime state and may contain prompt text, model output, command output, file paths, and other sensitive information. It is intentionally ignored by git. Do not commit it unless you have deliberately scrubbed and reviewed the contents.

A `.agents/skills` folder may exist in older or manually managed workspaces, but current app skill storage is backed by the SQLite database.

## Rules, Indexing, and Tasks

The rules engine currently ships with a baked-in Human Code Rules manifest. In the UI, rule checks can add warning log entries before a prompt runs. They are useful reminders, not a substitute for review, tests, permissions, or source control.

The file indexer walks the active project and skips common runtime folders such as `.git`, `node_modules`, `.svelte-kit`, `target`, `.vscode`, and `.agents`. Symbol extraction is currently aimed at source navigation, not security enforcement.

Tasks are stored in SQLite and can be run manually or on intervals such as `1m`, `5m`, `30m`, `1h`, `1d`, or `once`. Interval execution depends on the desktop app staying open.

## Operational Safety

Recommended working habits:

- Run nTropy on a disposable branch or throwaway worktree when exploring.
- Keep secrets, production credentials, and unrelated personal files out of the active workspace.
- Review `git status` and `git diff` before and after every automation run.
- Prefer narrowly scoped prompts that name allowed files and forbidden areas.
- Keep provider CLI auth scoped to accounts and projects where automated local execution is acceptable.
- Back up important work before running broad refactors or scheduled tasks.
- Stop the active session from the UI or terminate the app if a CLI begins acting outside the intended scope.

The safest assumption is that a launched provider CLI can do anything you could do in that shell.

## Development Notes

Primary app files:

- `src/routes/+page.svelte` contains the desktop UI.
- `src-tauri/src/lib.rs` registers Tauri commands and project database switching.
- `src-tauri/src/cli_mediator.rs` handles provider routing and child process launch.
- `src-tauri/src/db` contains SQLite schema and queries.
- `src-tauri/src/rules_engine.rs` contains the built-in advisory rules manifest.
- `src-tauri/src/symbol_indexer.rs` contains workspace file indexing and Tree-sitter symbol extraction.

Useful checks:

```powershell
npm run check
npm run build
cd src-tauri
cargo check
```
