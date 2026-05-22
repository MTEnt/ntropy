# nTropy

nTropy is a cross-platform local desktop control panel for running installed AI coding CLIs against a selected workspace. It wraps a Svelte 5 interface in Tauri 2, stores project state in local SQLite, indexes source files for navigation, and launches Claude, Gemini, Grok, or Codex child processes with broad local permissions on Windows, macOS, and Linux.

It is not an AI provider, hosted agent platform, or sandbox. nTropy is an orchestration shell around provider CLIs that are already installed and authenticated on the machine.

## What It Is For

nTropy is built for local AI-assisted development where the user wants one desktop app to:

- Open or create a project folder.
- Chat with a selected AI CLI inside that folder.
- Choose the active chat provider and underlying model.
- Persist those model choices across restarts.
- Delegate non-casual work to canonical companion agents.
- Keep raw terminal output visible without polluting the main chat.
- Store project conversations, tasks, and skills locally.
- Browse files and source symbols from the active workspace.
- Run repeatable local automation prompts while the app is open.

The product goal is direct local control: choose the CLI, choose the model, choose the workspace, and let the installed tooling operate there with the same operating-system permissions as the signed-in user.

## Current Capabilities

### Desktop App Shell

- Svelte 5 and SvelteKit power the UI.
- Tauri 2 wraps the UI as the nTropy desktop app.
- The real desktop app is launched with `npm run tauri dev`.
- The frontend-only Vite server can be launched with `npm run dev`, but it cannot exercise Tauri commands such as folder selection, SQLite project state, or CLI process launching.
- Vite is pinned to `http://localhost:1420` with `strictPort: true` because Tauri points to that URL during development.

### Project Management

- The app can open an existing project folder through a native folder picker.
- The app can create a new project folder.
- Opening or creating a project ensures the folder has:
  - `src/`
  - `.agents/`
  - `README.md`
- Each project gets local runtime state under `.agents/nentropy.db`.
- Recent projects are remembered in browser local storage.
- The left sidebar shows projects, conversations, and active project files.
- Startup uses `NTROPY_WORKSPACE_ROOT` when set, then a detected development project root, then the operating system's app-data workspace.

Opening an existing folder can therefore mutate that folder by adding the project scaffolding above. That is intentional current behavior.

### Chat And Conversations

- The central panel provides the main chat workflow.
- User messages and final assistant messages are persisted to SQLite.
- Streaming placeholder messages are updated in the UI while the CLI runs and are persisted only after final output is available.
- The app cleans raw CLI output before showing it in chat. It strips common CLI metadata, prompt echoes, system instruction blocks, token report lines, and orchestration logs.
- Raw stdout and stderr still go to the Terminal Stdout Log dock so debugging details are not lost.
- The chat input is locked while the main orchestrator session is running.

### Provider And Model Selection

nTropy currently supports four provider families:

- Claude CLI
- Gemini CLI
- Grok CLI
- Codex CLI

The active chat provider and underlying model are selected in the top toolbar. These selections are not just cosmetic: they are sent to the Rust backend in each `run_cli_prompt` request, validated against backend allowlists, and used when spawning the child process.

Model preferences are persisted in browser local storage under `ntropyModelPreferences`, including:

- Active chat provider.
- Provider-specific default model choices.
- Per-conversation selected provider.
- Subagent provider and model assignments.

Provider model availability can still drift because the real source of truth is the installed CLI and the authenticated account. A model can appear in the UI and still fail if the CLI or account does not support it.

Current frontend/backend model families include:

- Claude: Sonnet and Opus entries currently exposed by the app.
- Gemini: provider default, Gemini auto modes, Gemini 3 preview entries, Gemini 2.5 entries, and Gemma entries exposed through the Gemini CLI model picker.
- Grok: `grok-build` in the current app allowlist.
- Codex: `gpt-5.5`, `gpt-5.3-codex`, `o3-pro`, and `o3`.

### Routing

The backend chooses the provider for a prompt using deterministic routing:

- A prompt starting with `@claude`, `@gemini`, `@grok`, or `@codex` routes to that provider.
- Research-agent language routes through the research mapping, defaulting to Grok.
- Backend-agent language routes through the backend mapping, defaulting to Codex.
- Frontend-agent language routes through the frontend mapping, defaulting to Claude.
- Verification-agent language routes through the verification mapping, defaulting to Gemini.
- Otherwise, the selected active chat provider is used.

The UI also sends provider/model registries to the backend so the spawned process uses the user's selected model for that role when possible.

## Subagents And Orchestration

nTropy has four canonical companion agent roles:

| Agent key | Default role | Default provider family |
| --- | --- | --- |
| `research` | Codebase search, symbol lookup, investigation, and external research support | Grok |
| `backend` | Rust, Tauri, filesystem, command execution, and backend data flow | Codex |
| `frontend` | Svelte, TypeScript, interaction design, and styling | Claude |
| `verification` | Checks, tests, validation, and drift reporting | Gemini |

These are the only recognized subagent keys. Invented names such as `search`, `infra`, `ui-builder`, or provider names used as agent names are ignored by the canonical delegation layer.

### Mandatory Delegation

Casual chat can be answered by the orchestrator directly. Non-casual work is treated as delegation work. That includes prompts involving:

- Building or modifying an app.
- Reading or changing files.
- Debugging logs or errors.
- Running tests or verification.
- Researching behavior.
- Auth or model-routing failures.
- Backend, frontend, project, task, CLI, or Tauri work.

For these prompts, the coordinator is instructed to delegate using exact stdout lines:

```text
SPAWN_SUBAGENT:agent=<research|backend|frontend|verification>,task=<specific task>
```

The frontend also performs deterministic fan-out for relevant canonical roles so delegation does not depend only on the model choosing to print the spawn line.

### Spawn Boundaries

- Only the top-level main chat session can create accepted subagent spawn events.
- Worker sessions are told not to spawn more subagents.
- Nested `SPAWN_SUBAGENT` lines from workers are ignored.
- Spawn events are tied to the parent run id.
- The frontend rejects stale, duplicate, or parentless spawn events.
- Automatic fan-out is limited to one live run per canonical role for a single parent request.

The Rust backend parses valid spawn lines and emits a Tauri event. The Svelte frontend receives that event, validates it, builds the worker prompt, and calls the backend again to launch the selected provider/model for that canonical role.

## Full-Permission CLI Execution

Full-permission execution is the core behavior of this app.

The backend starts provider CLIs as child processes with the active project as the process working directory. That working directory gives the model local project context and relative paths, but it is not an operating-system sandbox.

Current high-permission flags include:

| Provider | Launch posture |
| --- | --- |
| Claude | `--print`, `--dangerously-skip-permissions`, `--permission-mode bypassPermissions` |
| Gemini | `--skip-trust`, `--approval-mode yolo` |
| Grok | `--always-approve`, `--permission-mode bypassPermissions` |
| Codex | `exec`, `--ignore-user-config`, `--disable plugins`, `--disable remote_plugin`, `--disable shell_snapshot`, `--dangerously-bypass-approvals-and-sandbox` |

Claude and Codex receive prompts through stdin. Grok and Gemini receive prompts as command arguments.

nTropy resolves provider CLIs in a platform-aware order:

- Provider-specific env var overrides first: `NTROPY_CLAUDE_BIN`, `NTROPY_GEMINI_BIN`, `NTROPY_GROK_BIN`, and `NTROPY_CODEX_BIN`.
- Gemini JS entrypoint override: `NTROPY_GEMINI_JS`.
- Known user-local install paths for the current OS, including Windows npm/global paths, Grok's user bin folder, common macOS Homebrew paths, common Linux paths, and npm-global folders.
- The normal command names from `PATH`, such as `claude`, `gemini`, `grok`, and `codex`.

The backend also augments `PATH` with common user bin folders before spawning, which helps macOS/Linux GUI launches find CLIs installed through Homebrew, npm global installs, Cargo, Bun, or user-local bin directories.

Use nTropy only with folders and accounts where this level of local automation is acceptable.

## Terminal Output And Token Tracking

nTropy intentionally separates the human chat from the raw process log:

- The main chat shows the cleaned user-facing response.
- The Terminal Stdout Log dock shows streamed stdout/stderr from the active CLI processes.
- Token reports are parsed when available and grouped by provider/model.
- Token usage summaries appear in the runtime UI and subagent panel.
- Terminal logs are currently in-memory UI state, not durable first-class database records.

This makes the chat readable while preserving enough raw output to debug failures.

## Tasks

The Tasks view supports saved local prompt automation:

- Create a task with a name, prompt/command, CLI provider, and schedule.
- Run one-shot tasks manually.
- Use interval schedules such as `1m`, `5m`, `30m`, `1h`, and `1d`.
- Store task definitions and status in SQLite.
- Run task prompts through the same CLI mediator as normal chat prompts.

Important limitations:

- Interval scheduling is implemented by the running frontend app.
- The desktop app must stay open for interval tasks to run.
- Task output history is not stored as a full durable audit log in the current schema.

## Rules

nTropy ships with an advisory Human Code Rules v5.1 manifest. The rules panel displays checks such as:

- Inspect relevant code before editing.
- Avoid unrelated changes.
- Do not rewrite architecture without cause.
- Justify new dependencies.
- Preserve tests.
- Be honest about verification.
- Treat security and correctness as first-class concerns.
- Report uncertainty.

The current rules engine is a warning system, not a hard policy engine. It can add warnings to the UI before a prompt runs, but it does not provide strong enforcement, rollback, sandboxing, or formal proof that the CLI stayed inside a rule boundary.

## Source Indexing

The backend includes a Tree-sitter symbol indexer for source navigation.

Supported file families:

- Rust
- TypeScript and TSX
- JavaScript and JSX
- Python

The indexer returns symbol names, kinds, line numbers, and byte ranges. It is useful for navigation and context gathering. It is not a security boundary.

The project file browser skips common runtime and generated folders, including:

- `.git`
- `.agents`
- `.svelte-kit`
- `.vscode`
- `node_modules`
- `target`

## Skills

Skills are stored in the local SQLite database. The UI can load the skills index and display full skill text. When the skills table is empty, the backend seeds default examples such as:

- Safe Git Push
- HTML Prompt Refactor

The backend exposes a `save_skill` command, but the current visible Svelte UI is primarily a skills viewer rather than a complete skill-authoring interface.

## Local Storage And Generated State

nTropy stores state in two places:

### Browser Local Storage

Used for UI preferences such as:

- Recent project list.
- Active chat provider/model preferences.
- Subagent provider/model preferences.
- Per-conversation model mapping.

### Project SQLite Database

Each active project uses:

```text
.agents/nentropy.db
```

SQLite WAL mode may also create:

```text
.agents/nentropy.db-shm
.agents/nentropy.db-wal
```

The database stores:

- Sessions.
- Messages.
- Tasks.
- Skills.
- Schema migration history.

The database may contain prompt text, model output, file paths, task text, and other sensitive local context. It is ignored by git and is not encrypted by this app.

Other generated local folders include:

- `node_modules/`
- `.svelte-kit/`
- `build/`
- `src-tauri/target/`
- `src-tauri/gen/`

## Backend Command Surface

The Tauri backend exposes commands for:

- Running a CLI prompt.
- Terminating a CLI session.
- Loading and checking rules.
- Indexing symbols.
- Listing, reading, and saving skills.
- Getting, opening, and creating projects.
- Listing project files.
- Selecting a directory.
- Creating, loading, updating, and deleting sessions/messages.
- Creating, updating, loading, and deleting tasks.

The powerful behavior comes from these custom Tauri invoke commands and spawned external CLIs, not from broad built-in Tauri plugin permissions.

## What nTropy Does Not Do

nTropy does not currently provide:

- OS-level sandboxing.
- Filesystem containment.
- Network isolation.
- Rollback.
- A durable background daemon.
- Encrypted local storage.
- A provider-hosted agent runtime.
- A guarantee that listed models are available to your account.
- A hard security policy engine.
- Full audit-log storage for terminal output and task results.
- Protection from a provider CLI reading or writing outside the active workspace if that CLI has permission to do so.

The selected workspace is the child process working directory, not a jail.

## Operational Safety

Recommended habits:

- Use disposable branches or throwaway worktrees for risky automation.
- Keep production secrets and unrelated personal files out of the active workspace.
- Review `git status` and `git diff` before and after runs.
- Keep prompts narrow and explicit about allowed files and forbidden areas.
- Back up important work before broad refactors or scheduled tasks.
- Keep provider CLI auth scoped to accounts where full-permission automation is acceptable.
- Stop the active session or close the app if a CLI begins operating outside the intended scope.

The safest assumption is simple: a launched provider CLI can do anything you could do in that shell.

## Setup

Install JavaScript dependencies:

```powershell
npm install
```

Install and authenticate the provider CLIs you plan to use:

- Claude Code CLI, available as `claude`.
- Gemini CLI, available as `gemini`.
- Grok CLI, available as `grok`.
- Codex CLI, available as `codex`.

On Windows, macOS, and Linux, nTropy can use either PATH-resolved CLI names or explicit env var overrides. If a provider fails to launch, verify the installed CLI path, login state, non-interactive mode support, and model availability.

## Running

Run the real desktop app:

```powershell
npm run tauri dev
```

Run only the Svelte/Vite frontend:

```powershell
npm run dev
```

Build frontend assets:

```powershell
npm run build
```

Preview built frontend assets:

```powershell
npm run preview
```

## Verification

Run Svelte checks:

```powershell
npm run check
```

Run frontend build:

```powershell
npm run build
```

Run Rust checks:

```powershell
cd src-tauri
cargo check
```

## Source Map

Primary implementation files:

- `src/routes/+page.svelte` - main desktop UI, chat, subagent panel, tasks, rules, skills, token summaries, and terminal log dock.
- `src-tauri/src/lib.rs` - Tauri command registration, app state, project switching, project scaffolding, sessions, messages, tasks, and skills.
- `src-tauri/src/cli_mediator.rs` - provider routing, model validation, prompt harnessing, CLI launch, streaming, termination, and subagent spawn parsing.
- `src-tauri/src/db/` - SQLite schema, migrations, and persistence queries.
- `src-tauri/src/rules_engine.rs` - built-in advisory rule manifest and warning checks.
- `src-tauri/src/symbol_indexer.rs` - Tree-sitter source parsing and symbol extraction.
- `src-tauri/tauri.conf.json` - Tauri product metadata, dev URL, window config, bundle config, and current CSP posture.
- `vite.config.js` - SvelteKit/Vite configuration for Tauri development.

## Security Posture

nTropy is a local trusted-user desktop tool. The current Tauri config has `csp: null`, and the app deliberately launches external AI CLIs with approval-bypass flags. That matches the current product intent, but it means nTropy should not be treated like a locked-down browser app, multi-tenant system, or untrusted-code runner.

Use it as a powerful local workbench, not as a security boundary.
