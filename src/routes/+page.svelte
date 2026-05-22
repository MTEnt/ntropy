<script lang="ts">
  import { onMount } from "svelte";
  import { invoke, isTauri } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";

  // Svelte 5 Runes for highly reactive state management
  let activeTab = $state("chat"); // Right-side panel tab: "chat" (subagents), "skills", "rules"
  let currentView = $state("chat"); // Main central panel view: "chat", "tasks"

  let prompt = $state("");
  let streamLogs = $state<string[]>([]);
  const MAIN_SESSION_ID = "active-workspace-session";
  const MODEL_PREFS_STORAGE_KEY = "ntropyModelPreferences";
  let activeStreamingMessageId = $state<string | null>(null);
  let streamingMessageBySession = $state<Record<string, string>>({});
  let streamingMetaBySession = $state<Record<string, StreamingSessionMeta>>({});
  let rawStreamingTextBySession = $state<Record<string, string>>({});
  let rawErrorTextBySession = $state<Record<string, string>>({});
  let promptBySession = $state<Record<string, string>>({});
  let sessionModelBySession = $state<Record<string, { provider: string, specificModel: string }>>({});
  let tokenUsageByModel = $state<Record<string, { provider: string, specificModel: string, totalTokens: number, lastTokens: number, actions: number }>>({});
  let delegatedAgentKeysBySession = $state<Record<string, Record<string, boolean>>>({});
  let storedConversationModels = $state<Record<string, string>>({});
  let streamTimeouts: Record<string, any> = {};
  let pendingTokenMarkerBySession: Record<string, boolean> = {};
  let preferredChatModel = $state("claude");
  const MAX_STREAM_LOG_LINES = 2000;

  function appendStreamLog(log: string) {
    streamLogs = [...streamLogs, log].slice(-MAX_STREAM_LOG_LINES);
  }

  function canUseTauriIpc(): boolean {
    const tauriInternals = (globalThis as any).__TAURI_INTERNALS__;
    return isTauri()
      && typeof tauriInternals?.invoke === "function"
      && typeof tauriInternals?.transformCallback === "function";
  }

  function createRunId(): string {
    if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
      return crypto.randomUUID();
    }
    return `run-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
  }

  function sanitizeStdout(text: string): string {
    // Strip ANSI colors, styling, cursor controls, etc.
    let cleaned = text.replace(/[\u001b\x1b]\[[0-9;?]*[a-zA-Z]/g, "");
    // Strip character set sequences like ESC ( B
    cleaned = cleaned.replace(/[\u001b\x1b]\([A-Z]/g, "");
    // Clean up carriage returns
    cleaned = cleaned.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
    
    // Filter out diagnostic/process termination line spam
    let lines = cleaned.split("\n");
    let filteredLines = lines.filter(line => {
      let trimmed = line.trim();
      
      // Filter out taskkill SUCCESS messages
      if (trimmed.startsWith("SUCCESS: The process") || trimmed.includes("has been terminated.")) {
        return false;
      }
      if (trimmed.includes("SPAWN_SUBAGENT:")) {
        return false;
      }
      
      return true;
    });

    return filteredLines.join("\n").trim();
  }

  function cleanCliText(text: string): string {
    return text
      .replace(/[\u001b\x1b]\[[0-9;?]*[a-zA-Z]/g, "")
      .replace(/[\u001b\x1b]\([A-Z]/g, "")
      .replace(/\r\n/g, "\n")
      .replace(/\r/g, "\n");
  }

  function isCliMetadataLine(trimmed: string): boolean {
    if (!trimmed) return false;
    if (trimmed.startsWith("[ORCHESTRATOR]")) return true;
    if (trimmed.startsWith("[stderr]")) return true;
    if (trimmed.includes("SPAWN_SUBAGENT:")) return true;
    if (/^-{5,}$/.test(trimmed)) return true;
    if (/^OpenAI Codex v/i.test(trimmed)) return true;
    if (/^(codex|gemini|grok|claude)$/i.test(trimmed)) return true;
    if (/^(workdir|model|provider|approval|sandbox|reasoning effort|reasoning summaries|session id):/i.test(trimmed)) return true;
    if (/^202\d-\d\d-\d\dT.*\b(ERROR|WARN)\b/i.test(trimmed)) return true;
    return false;
  }

  function cleanChatOutput(text: string, sessionId: string): string {
    const originalPrompt = promptBySession[sessionId] || "";
    const promptLines = new Set(
      originalPrompt
        .split("\n")
        .map(line => line.trim())
        .filter(Boolean)
    );
    const lines = cleanCliText(text).split("\n");
    const kept: string[] = [];
    let skipTokenNumber = false;
    let skipSystemBlock = false;
    let skipUserBlock = false;

    for (const line of lines) {
      const trimmed = line.trim();

      if (!trimmed) {
        if (kept.length > 0 && kept[kept.length - 1] !== "") kept.push("");
        continue;
      }

      if (isCliMetadataLine(trimmed)) continue;
      if (/^tokens used$/i.test(trimmed) || /^tokens?[: ]/i.test(trimmed)) {
        skipTokenNumber = true;
        continue;
      }
      if (skipTokenNumber && /^[\d,]+$/.test(trimmed)) {
        skipTokenNumber = false;
        continue;
      }
      skipTokenNumber = false;

      if (trimmed === "user") {
        skipUserBlock = true;
        continue;
      }
      if (skipUserBlock) {
        if (trimmed.includes("[SYSTEM INSTRUCTION]")) {
          skipUserBlock = false;
          skipSystemBlock = true;
        }
        continue;
      }

      if (trimmed.includes("[SYSTEM INSTRUCTION]")) {
        skipSystemBlock = true;
        continue;
      }
      if (skipSystemBlock) {
        if (/^(assistant|final answer|answer)$/i.test(trimmed)) {
          skipSystemBlock = false;
          continue;
        } else {
          continue;
        }
      }

      if (/^(assistant|final answer|answer)$/i.test(trimmed)) continue;
      if (promptLines.has(trimmed)) continue;
      if (isCliMetadataLine(trimmed)) continue;

      kept.push(line.trimEnd());
    }

    return kept.join("\n").trim();
  }

  function extractFallbackChatOutput(stderrText: string, sessionId: string): string {
    const cleaned = cleanCliText(stderrText);
    const tokenMarker = cleaned.toLowerCase().lastIndexOf("tokens used");
    if (tokenMarker >= 0) {
      const afterMarker = cleaned.slice(tokenMarker).split("\n");
      const tokenLineHasCount = /tokens used\s*[:=]?\s*[\d,]+/i.test(afterMarker[0] || "");
      const startIndex = tokenLineHasCount ? 1 : (/^[\d,]+$/.test((afterMarker[1] || "").trim()) ? 2 : 1);
      const afterTokenCount = afterMarker.slice(startIndex).join("\n");
      const visible = cleanChatOutput(afterTokenCount, sessionId);
      if (visible) return visible;
    }

    const errorLine = cleaned
      .split("\n")
      .map(line => line.trim())
      .find(line => !isCliMetadataLine(line) && /\b(error|failed|denied|read-only|permission|cannot|can't)\b/i.test(line));
    return errorLine || "";
  }

  function formatTokens(value: number): string {
    return value.toLocaleString();
  }

  function registerSessionModel(sessionId: string, provider: string, specificModel: string) {
    sessionModelBySession = {
      ...sessionModelBySession,
      [sessionId]: { provider, specificModel: specificModel || "provider-default" }
    };
  }

  function recordBackendModelSelection(sessionId: string, data: string) {
    const match = cleanCliText(data).match(/\[ORCHESTRATOR\]\s*Exact backend model selection:\s*provider=([^\s,]+)\s+specific_model=([^\s,]+)/i);
    if (match) {
      registerSessionModel(sessionId, match[1], match[2]);
    }
  }

  function addTokenUsage(sessionId: string, tokens: number) {
    if (!Number.isFinite(tokens) || tokens <= 0) return;
    const modelInfo = sessionModelBySession[sessionId] || { provider: "unknown", specificModel: "provider-default" };
    const key = `${modelInfo.provider}:${modelInfo.specificModel}`;
    const existing = tokenUsageByModel[key] || {
      provider: modelInfo.provider,
      specificModel: modelInfo.specificModel,
      totalTokens: 0,
      lastTokens: 0,
      actions: 0
    };
    tokenUsageByModel = {
      ...tokenUsageByModel,
      [key]: {
        ...existing,
        totalTokens: existing.totalTokens + tokens,
        lastTokens: tokens,
        actions: existing.actions + 1
      }
    };
  }

  function recordTokenUsage(sessionId: string, data: string) {
    const lines = cleanCliText(data).split("\n").map(line => line.trim()).filter(Boolean);
    for (const line of lines) {
      const sameLine = line.match(/tokens used\s*[:=]?\s*([\d,]+)/i);
      if (sameLine) {
        addTokenUsage(sessionId, Number(sameLine[1].replace(/,/g, "")));
        pendingTokenMarkerBySession[sessionId] = false;
        continue;
      }

      if (/^tokens used$/i.test(line)) {
        pendingTokenMarkerBySession[sessionId] = true;
        continue;
      }

      if (pendingTokenMarkerBySession[sessionId] && /^[\d,]+$/.test(line)) {
        addTokenUsage(sessionId, Number(line.replace(/,/g, "")));
        pendingTokenMarkerBySession[sessionId] = false;
      }
    }
  }
  
  // Rules State
  let rulesList = $state<Array<{ id: string, severity: string, trigger: string, text: string }>>([]);
  let ruleCheckList = $state<Record<string, boolean>>({});
  
  // File Explorer State
  let files = $state<string[]>([]);
  let selectedFile = $state("src-tauri/src/lib.rs");
  let fileSymbols = $state<Array<{ name: string, kind: string, line_number: number }>>([]);

  // Project Workspace State
  let currentProjectPath = $state("");
  let recentProjects = $state<string[]>([]);

  // Clean UI layout states matching nTropy
  let showStartupChooser = $state(true);
  let showFilesSection = $state(true);
  let showRightPanel = $state(false);
  let showSidebar = $state(true);

  let currentProjectName = $derived(
    currentProjectPath ? (currentProjectPath.split('\\').pop() || currentProjectPath.split('/').pop() || currentProjectPath) : "No Workspace"
  );

  // Conversations & Multi-Chat per Project State
  interface Message {
    id?: string;
    sender: string;
    text: string;
    type: 'user' | 'agent' | 'system';
  }

  interface Conversation {
    id: string;
    projectPath: string;
    title: string;
    lastUpdated: number;
    messages: Message[];
    activeModel: string;
  }

  interface StreamingSessionMeta {
    conversationId: string;
    messageId: string;
    runId: string;
    sender: string;
    type: 'user' | 'agent' | 'system';
    subagentId?: string;
    parentSessionId?: string;
    persistedFinal?: boolean;
  }

  interface CliFinishedPayload {
    sessionId: string;
    success: boolean;
    exitCode?: number | null;
    error?: string;
  }

  interface Subagent {
    id: string;
    name: string;
    role: string;
    model: string;
    specificModel: string;
    active: boolean;
    cost: string;
  }

  interface StoredSubagentPreference {
    id: string;
    model: string;
    specificModel: string;
    active: boolean;
  }

  interface ModelPreferences {
    activeChatModel?: string;
    providerModels?: Record<string, string>;
    subagents?: StoredSubagentPreference[];
    conversationModels?: Record<string, string>;
  }

  interface BackendSubagentModelSelection {
    name: string;
    role: string;
    provider: string;
    specificModel: string;
    active: boolean;
  }

  type AgentKey = "research" | "backend" | "frontend" | "verification";

  let conversations = $state<Conversation[]>([]);
  let activeConversationId = $state<string | null>(null);
  let showContextMenu = $state(false);
  let contextMenuX = $state(0);
  let contextMenuY = $state(0);
  let contextMenuConvId = $state<string | null>(null);

  // Derived active conversation
  let activeConversation = $derived(
    conversations.find(c => c.id === activeConversationId) || null
  );

  // Derived active conversation messages
  let chatMessages = $derived(
    activeConversation ? activeConversation.messages : []
  );

  // Derived active conversation model
  let activeModel = $derived(
    activeConversation ? activeConversation.activeModel : preferredChatModel
  );

  // Scheduled Tasks State
  interface ScheduledTask {
    id: string;
    projectPath: string;
    name: string;
    command: string;
    cli: string;
    schedule: string; // "1m", "5m", "30m", "1h", "1d", "once"
    status: 'active' | 'paused' | 'running';
    lastRun: number | null;
    lastResult: 'success' | 'failed' | null;
    lastOutput: string;
  }

  let tasks = $state<ScheduledTask[]>([]);

  function getProjectFirstMessagePreview(path: string): string {
    const projConvs = conversations.filter(c => c.projectPath === path);
    if (projConvs.length === 0) return "New Conversation";
    // Sort by lastUpdated descending and pick the most recent one
    const sorted = [...projConvs].sort((a, b) => b.lastUpdated - a.lastUpdated);
    
    // Find the last user or agent message in that conversation
    const messages = sorted[0].messages;
    const lastUser = messages.slice().reverse().find((m: any) => m.type === 'user');
    if (lastUser) {
      return lastUser.text.length > 25 ? lastUser.text.substring(0, 25) + "..." : lastUser.text;
    }
    const lastAgent = messages.slice().reverse().find((m: any) => m.type === 'agent');
    if (lastAgent) {
      return lastAgent.text.length > 25 ? lastAgent.text.substring(0, 25) + "..." : lastAgent.text;
    }
    return "New Conversation";
  }

  function getProjectTimeText(path: string): string {
    const projConvs = conversations.filter(c => c.projectPath === path);
    if (projConvs.length === 0) return "new";
    const sorted = [...projConvs].sort((a, b) => b.lastUpdated - a.lastUpdated);
    return getConvTimeText(sorted[0].lastUpdated);
  }

  function getConvTimeText(timestamp: number): string {
    const elapsed = Date.now() - timestamp;
    const secs = Math.floor(elapsed / 1000);
    if (secs < 60) return "now";
    const mins = Math.floor(secs / 60);
    if (mins < 60) return `${mins}m`;
    const hours = Math.floor(mins / 60);
    if (hours < 24) return `${hours}h`;
    const days = Math.floor(hours / 24);
    return `${days}d`;
  }

  async function handleChoosePathDirect() {
    try {
      const selected: string = await invoke("select_directory");
      if (selected) {
        await handleOpenProject(selected);
      }
    } catch (e) {
      console.log("Folder selector closed or failed:", e);
    }
  }

  async function loadProjectDatabaseState() {
    if (!currentProjectPath) return;
    try {
      // 1. Load sessions from SQLite
      const dbSessions: any[] = await invoke("get_all_sessions");
      
      const loadedConvs: Conversation[] = [];
      for (const s of dbSessions) {
        const [id, title, created_at, updated_at] = s;
        
        // Fetch messages for this session
        const dbMsgs: any[] = await invoke("get_session_messages", { sessionId: id });
        const messages = dbMsgs.map((m: any) => {
          const [role, content, msg_created_at] = m;
          let sender = "System";
          let type: 'user' | 'agent' | 'system' = 'system';
          if (role.includes('|')) {
            const parts = role.split('|');
            sender = parts[0];
            type = parts[1] as any;
          } else {
            sender = role;
            if (role === "User") type = 'user';
            else if (role.toLowerCase().includes("agent")) type = 'agent';
            else type = 'system';
          }
          return { sender, text: content, type };
        });
        
        loadedConvs.push({
          id,
          projectPath: currentProjectPath,
          title,
          lastUpdated: new Date(updated_at).getTime(),
          messages,
          activeModel: getSavedConversationModel(id)
        });
      }
      
      conversations = loadedConvs;
      
      if (conversations.length > 0) {
        activeConversationId = conversations[0].id;
      } else {
        await createNewConversation(currentProjectPath);
      }
      
      // 2. Load tasks from SQLite
      const dbTasks: any[] = await invoke("get_all_tasks");
      tasks = dbTasks.map((t: any) => {
        const [id, session_id, text, status, created_at, updated_at] = t;
        let name = text;
        let command = text;
        let cli = "claude";
        let schedule = "once";
        try {
          if (text.startsWith("{") && text.endsWith("}")) {
            const parsed = JSON.parse(text);
            name = parsed.name || text;
            command = parsed.command || text;
            cli = parsed.cli || "claude";
            schedule = parsed.schedule || "once";
          }
        } catch (e) {
          // Fallback if not JSON
        }
        return {
          id,
          projectPath: currentProjectPath,
          name,
          command,
          cli,
          schedule,
          status: status === "active" ? "active" : (status === "paused" ? "paused" : "running"),
          lastRun: new Date(updated_at).getTime(),
          lastResult: status === "completed" ? "success" : (status === "failed" ? "failed" : null),
          lastOutput: ""
        };
      });
    } catch (e) {
      console.error("Failed to load project database state:", e);
    }
  }

  async function createNewConversation(projectPath: string, initialTitle = "New Conversation"): Promise<string> {
    const newId = `conv-${Date.now()}-${Math.random().toString(36).substr(2, 9)}`;
    const newConv: Conversation = {
      id: newId,
      projectPath,
      title: initialTitle,
      lastUpdated: Date.now(),
      messages: [],
      activeModel: preferredChatModel
    };
    storedConversationModels = { ...storedConversationModels, [newId]: preferredChatModel };
    conversations = [newConv, ...conversations];
    activeConversationId = newId;
    
    try {
      await invoke("create_session", { id: newId, title: initialTitle });
    } catch (e) {
      console.error("Failed to create session in SQLite:", e);
    }
    saveModelPreferences();
    
    return newId;
  }

  async function handleNewConversationClick() {
    if (!currentProjectPath) return;
    await createNewConversation(currentProjectPath);
    currentView = "chat";
  }

  function handleContextMenu(e: MouseEvent, convId: string) {
    e.preventDefault();
    e.stopPropagation();
    contextMenuConvId = convId;
    contextMenuX = e.clientX;
    contextMenuY = e.clientY;
    showContextMenu = true;
  }

  async function deleteConversation(id: string) {
    conversations = conversations.filter(c => c.id !== id);
    if (activeConversationId === id) {
      const currentProjConvs = conversations.filter(c => c.projectPath === currentProjectPath);
      if (currentProjConvs.length > 0) {
        activeConversationId = currentProjConvs[0].id;
      } else if (conversations.length > 0) {
        activeConversationId = conversations[0].id;
        currentProjectPath = conversations[0].projectPath;
      } else {
        activeConversationId = null;
      }
    }
    try {
      await invoke("delete_session", { id });
    } catch (e) {
      console.error("Failed to delete session in SQLite:", e);
    }
    showContextMenu = false;
  }

  async function persistConversationMessage(conversationId: string, sender: string, text: string, type: 'user' | 'agent' | 'system') {
    try {
      await invoke("add_session_message", {
        sessionId: conversationId,
        role: `${sender}|${type}`,
        content: text
      });
    } catch (e) {
      console.error("Failed to add message to SQLite:", e);
    }
  }

  async function addMessageToConversation(
    conversationId: string,
    sender: string,
    text: string,
    type: 'user' | 'agent' | 'system',
    id?: string,
    persist = true
  ) {
    let didAdd = false;
    conversations = conversations.map(c => {
      if (c.id === conversationId) {
        didAdd = true;
        let title = c.title;
        if (title === "New Conversation" && type === "user") {
          title = text.length > 25 ? text.substring(0, 25) + "..." : text;
          invoke("create_session", { id: c.id, title });
        }
        return {
          ...c,
          title,
          lastUpdated: Date.now(),
          messages: [...c.messages, { id, sender, text, type }]
        };
      }
      return c;
    });

    if (persist && didAdd) {
      await persistConversationMessage(conversationId, sender, text, type);
    }
  }

  async function addMessageToActiveConversation(sender: string, text: string, type: 'user' | 'agent' | 'system', id?: string, persist = true) {
    if (!activeConversationId) return;
    await addMessageToConversation(activeConversationId, sender, text, type, id, persist);
  }

  function updateStreamingMessageText(msgId: string, text: string, conversationId = activeConversationId) {
    if (!conversationId) return;
    conversations = conversations.map(c => {
      if (c.id === conversationId) {
        return {
          ...c,
          lastUpdated: Date.now(),
          messages: c.messages.map(m => {
            if (m.id === msgId) {
              return { ...m, text };
            }
            return m;
          })
        };
      }
      return c;
    });
  }

  function normalizeCliFinishedPayload(payload: any): CliFinishedPayload | null {
    if (typeof payload === "string") {
      return { sessionId: payload, success: true };
    }
    if (!payload || typeof payload !== "object") return null;

    const sessionId = String(payload.session_id || payload.sessionId || payload.id || "");
    if (!sessionId) return null;

    const exitCodeRaw = payload.exit_code ?? payload.exitCode ?? payload.code;
    const exitCode = exitCodeRaw === undefined || exitCodeRaw === null ? null : Number(exitCodeRaw);
    let success = true;
    if (typeof payload.success === "boolean") {
      success = payload.success;
    } else if (typeof payload.ok === "boolean") {
      success = payload.ok;
    } else if (typeof payload.status === "string") {
      success = !/\b(fail|failed|error|errored|cancel|cancelled)\b/i.test(payload.status);
    } else if (Number.isFinite(exitCode)) {
      success = exitCode === 0;
    } else if (payload.error) {
      success = false;
    }

    return {
      sessionId,
      success,
      exitCode: Number.isFinite(exitCode) ? exitCode : null,
      error: typeof payload.error === "string"
        ? payload.error
        : (typeof payload.message === "string" ? payload.message : undefined)
    };
  }

  function formatFinishedStatus(finished: CliFinishedPayload): string {
    if (finished.success) return "finished successfully";
    const exitText = finished.exitCode === null || finished.exitCode === undefined ? "" : ` with exit code ${finished.exitCode}`;
    const errorText = finished.error ? `: ${finished.error}` : "";
    return `failed${exitText}${errorText}`;
  }

  function failureFallbackForFinished(finished: CliFinishedPayload): string {
    return `[Process ${formatFinishedStatus(finished)}. Check the terminal log for details.]`;
  }

  function hasLiveSubagentSession(subagentId: string): boolean {
    return Object.values(streamingMetaBySession).some(meta => meta.subagentId === subagentId);
  }

  function setSubagentActive(subagentId: string, active: boolean) {
    subagents = subagents.map(existing => existing.id === subagentId ? { ...existing, active } : existing);
  }

  function registerStreamingSession(
    sessionId: string,
    messageId: string,
    sourcePrompt: string,
    provider: string,
    specificModel: string,
    conversationId: string,
    sender: string,
    type: 'user' | 'agent' | 'system' = 'agent',
    subagentId?: string,
    parentSessionId?: string,
    runId = createRunId()
  ): string {
    streamingMessageBySession = { ...streamingMessageBySession, [sessionId]: messageId };
    streamingMetaBySession = {
      ...streamingMetaBySession,
      [sessionId]: {
        conversationId,
        messageId,
        runId,
        sender,
        type,
        subagentId,
        parentSessionId,
        persistedFinal: false
      }
    };
    rawStreamingTextBySession = { ...rawStreamingTextBySession, [sessionId]: "" };
    rawErrorTextBySession = { ...rawErrorTextBySession, [sessionId]: "" };
    promptBySession = { ...promptBySession, [sessionId]: sourcePrompt };
    registerSessionModel(sessionId, provider, specificModel);
    if (sessionId === MAIN_SESSION_ID) {
      activeStreamingMessageId = messageId;
    }
    return runId;
  }

  function clearStreamingSession(sessionId: string) {
    const meta = streamingMetaBySession[sessionId];
    const { [sessionId]: _msg, ...remainingMessages } = streamingMessageBySession;
    const { [sessionId]: _meta, ...remainingMeta } = streamingMetaBySession;
    const { [sessionId]: _raw, ...remainingRaw } = rawStreamingTextBySession;
    const { [sessionId]: _err, ...remainingErrors } = rawErrorTextBySession;
    const { [sessionId]: _prompt, ...remainingPrompts } = promptBySession;
    const { [sessionId]: _delegated, ...remainingDelegated } = delegatedAgentKeysBySession;
    streamingMessageBySession = remainingMessages;
    streamingMetaBySession = remainingMeta;
    rawStreamingTextBySession = remainingRaw;
    rawErrorTextBySession = remainingErrors;
    promptBySession = remainingPrompts;
    delegatedAgentKeysBySession = remainingDelegated;
    delete pendingTokenMarkerBySession[sessionId];
    if (streamTimeouts[sessionId]) {
      clearTimeout(streamTimeouts[sessionId]);
      delete streamTimeouts[sessionId];
    }
    if (sessionId === MAIN_SESSION_ID) {
      activeStreamingMessageId = null;
    }
    if (meta?.subagentId) {
      const stillRunningSameAgent = Object.entries(remainingMeta).some(([, other]) => other.subagentId === meta.subagentId);
      if (!stillRunningSameAgent) {
        setSubagentActive(meta.subagentId, false);
      }
    }
  }

  function appendStreamingChunk(sessionId: string, stream: string, data: string) {
    const msgId = streamingMessageBySession[sessionId];
    const meta = streamingMetaBySession[sessionId];
    if (!msgId || !meta) return;

    recordBackendModelSelection(sessionId, data);
    recordTokenUsage(sessionId, data);

    if (stream === "stderr") {
      const nextError = `${rawErrorTextBySession[sessionId] || ""}${data}`;
      rawErrorTextBySession = { ...rawErrorTextBySession, [sessionId]: nextError };
    } else {
      const nextRaw = `${rawStreamingTextBySession[sessionId] || ""}${data}`;
      rawStreamingTextBySession = { ...rawStreamingTextBySession, [sessionId]: nextRaw };
      const visible = cleanChatOutput(nextRaw, sessionId);
      if (visible) {
        updateStreamingMessageText(msgId, visible, meta.conversationId);
      }
    }

    if (streamTimeouts[sessionId]) clearTimeout(streamTimeouts[sessionId]);
    streamTimeouts[sessionId] = setTimeout(() => {
      const stillStreamingMsgId = streamingMessageBySession[sessionId];
      const stillStreamingMeta = streamingMetaBySession[sessionId];
      if (!stillStreamingMsgId || !stillStreamingMeta) return;
      const visible = cleanChatOutput(rawStreamingTextBySession[sessionId] || "", sessionId)
        || extractFallbackChatOutput(rawErrorTextBySession[sessionId] || "", sessionId);
      updateStreamingMessageText(
        stillStreamingMsgId,
        visible || "[Still running. Check the Terminal Stdout Log for live details.]",
        stillStreamingMeta.conversationId
      );
    }, 120000);
  }

  async function persistFinalStreamingMessage(sessionId: string, text: string) {
    const meta = streamingMetaBySession[sessionId];
    if (!meta || meta.persistedFinal || !text.trim()) return;
    streamingMetaBySession = {
      ...streamingMetaBySession,
      [sessionId]: { ...meta, persistedFinal: true }
    };
    await persistConversationMessage(meta.conversationId, meta.sender, text, meta.type);
  }

  async function finishStreamingSession(sessionId: string, emptyFallback = "[Process finished without producing chat output. Check the terminal log for details.]") {
    const msgId = streamingMessageBySession[sessionId];
    const meta = streamingMetaBySession[sessionId];
    try {
      if (msgId && meta) {
        const finalText = cleanChatOutput(rawStreamingTextBySession[sessionId] || "", sessionId)
          || extractFallbackChatOutput(rawErrorTextBySession[sessionId] || "", sessionId);
        const persistedText = finalText || emptyFallback;
        updateStreamingMessageText(msgId, persistedText, meta.conversationId);
        await persistFinalStreamingMessage(sessionId, persistedText);
      }
    } finally {
      clearStreamingSession(sessionId);
    }
  }

  function loadRecentProjects() {
    try {
      const stored = localStorage.getItem("recentProjects");
      if (stored) {
        recentProjects = JSON.parse(stored);
      } else {
        recentProjects = [];
        if (currentProjectPath) {
          recentProjects = [currentProjectPath];
          saveRecentProjects();
        }
      }
    } catch (e) {
      console.error("Failed to load recent projects:", e);
    }
  }

  function saveRecentProjects() {
    try {
      localStorage.setItem("recentProjects", JSON.stringify(recentProjects));
    } catch (e) {
      console.error("Failed to save recent projects:", e);
    }
  }

  function addRecentProject(path: string) {
    if (!path) return;
    recentProjects = recentProjects.filter(p => p !== path);
    recentProjects = [path, ...recentProjects].slice(0, 5);
    saveRecentProjects();
  }

  async function refreshFiles() {
    try {
      files = await invoke("list_project_files");
      if (files.length > 0) {
        if (!files.includes(selectedFile)) {
          selectedFile = files[0];
        }
        await loadSymbols(selectedFile);
      } else {
        fileSymbols = [];
      }
    } catch (e) {
      console.error("Failed to list project files:", e);
    }
  }

  async function handleOpenProject(path: string) {
    if (!path || !path.trim()) return;
    try {
      const resFiles: string[] = await invoke("open_project", { path });
      files = resFiles;
      currentProjectPath = path;
      addRecentProject(path);
      
      // Load everything from the SQLite database
      await loadProjectDatabaseState();
      
      if (files.length > 0) {
        selectedFile = files[0];
        await loadSymbols(selectedFile);
      } else {
        fileSymbols = [];
      }
      
      await refreshSkills();
    } catch (e) {
      console.error("Failed to open project:", e);
      conversations = [{
        id: `conv-fallback`,
        projectPath: path,
        title: "Workspace Load Error",
        lastUpdated: Date.now(),
        messages: [{ sender: "Workspace Engine", text: `❌ Error opening project: ${e}`, type: 'system' }],
        activeModel: preferredChatModel
      }];
      activeConversationId = `conv-fallback`;
    }
  }
  
  // Model mapping configuration
  const providerModelsMap: Record<string, string[]> = {
    claude: ["provider-default", "claude-opus-4.7", "claude-sonnet-4.6", "claude-haiku-4.5"],
    gemini: [
      "provider-default",
      "auto-gemini-3",
      "auto-gemini-2.5",
      "gemini-3.1-pro-preview",
      "gemini-3-flash-preview",
      "gemini-3.1-flash-lite-preview",
      "gemini-2.5-pro",
      "gemini-2.5-flash",
      "gemini-2.5-flash-lite",
      "gemma-4-31b-it",
      "gemma-4-26b-a4b-it"
    ],
    grok: ["grok-build"],
    codex: ["provider-default", "gpt-5.5", "gpt-5.3-codex", "o3-pro", "o3"]
  };

  let defaultProviderModels = $state<Record<string, string>>({
    claude: "claude-sonnet-4.6",
    gemini: "auto-gemini-3",
    grok: "grok-build",
    codex: "gpt-5.5"
  });
  
  // Subagent Control Panel State
  let subagents = $state<Subagent[]>([
    { id: "sa-1", name: "Research Agent", role: "Codebase search & symbols", model: "grok", specificModel: "grok-build", active: false, cost: "$0.02" },
    { id: "sa-2b", name: "Backend Coder Agent", role: "Rust / API / backend services", model: "codex", specificModel: "gpt-5.3-codex", active: false, cost: "$0.03" },
    { id: "sa-2f", name: "Frontend Coder Agent", role: "Svelte / TS / styling design", model: "claude", specificModel: "claude-sonnet-4.6", active: false, cost: "$0.03" },
    { id: "sa-3", name: "Verification Agent", role: "Cargo check / test execution", model: "gemini", specificModel: "auto-gemini-3", active: false, cost: "$0.01" }
  ]);

  function isKnownProvider(provider: string): boolean {
    return Object.prototype.hasOwnProperty.call(providerModelsMap, provider);
  }

  function isValidProviderModel(provider: string, specificModel: string): boolean {
    return (providerModelsMap[provider] || []).includes(specificModel);
  }

  function getSafeSpecificModel(provider: string, requested?: string): string {
    if (requested && isValidProviderModel(provider, requested)) {
      return requested;
    }
    if (defaultProviderModels[provider] && isValidProviderModel(provider, defaultProviderModels[provider])) {
      return defaultProviderModels[provider];
    }
    return (providerModelsMap[provider] || [])[0] || "";
  }

  function formatSpecificModelLabel(specificModel: string): string {
    if (specificModel === "auto-gemini-3") return "Auto (Gemini 3)";
    if (specificModel === "auto-gemini-2.5") return "Auto (Gemini 2.5)";
    return specificModel === "provider-default" ? "Provider default / auto" : specificModel;
  }

  function getSavedConversationModel(conversationId: string): string {
    const saved = storedConversationModels[conversationId];
    return saved && isKnownProvider(saved) ? saved : preferredChatModel;
  }

  function loadModelPreferences() {
    try {
      const stored = localStorage.getItem(MODEL_PREFS_STORAGE_KEY);
      if (!stored) return;

      const prefs = JSON.parse(stored) as ModelPreferences;
      if (prefs.activeChatModel && isKnownProvider(prefs.activeChatModel)) {
        preferredChatModel = prefs.activeChatModel;
      }

      if (prefs.providerModels) {
        const nextProviderModels = { ...defaultProviderModels };
        for (const [provider, specificModel] of Object.entries(prefs.providerModels)) {
          if (isKnownProvider(provider) && isValidProviderModel(provider, specificModel)) {
            nextProviderModels[provider] = specificModel;
          }
        }
        defaultProviderModels = nextProviderModels;
      }

      if (Array.isArray(prefs.subagents)) {
        const prefsById = new Map(prefs.subagents.map(sa => [sa.id, sa]));
        subagents = subagents.map(sa => {
          const saved = prefsById.get(sa.id);
          if (!saved || !isKnownProvider(saved.model)) return sa;
          return {
            ...sa,
            model: saved.model,
            specificModel: getSafeSpecificModel(saved.model, saved.specificModel),
            active: false
          };
        });
      }

      if (prefs.conversationModels) {
        const nextConversationModels: Record<string, string> = {};
        for (const [conversationId, provider] of Object.entries(prefs.conversationModels)) {
          if (isKnownProvider(provider)) {
            nextConversationModels[conversationId] = provider;
          }
        }
        storedConversationModels = nextConversationModels;
      }
    } catch (e) {
      console.error("Failed to load model preferences:", e);
    }
  }

  function saveModelPreferences() {
    try {
      const conversationModels: Record<string, string> = { ...storedConversationModels };
      for (const conversation of conversations) {
        if (isKnownProvider(conversation.activeModel)) {
          conversationModels[conversation.id] = conversation.activeModel;
        }
      }
      const prefs: ModelPreferences = {
        activeChatModel: preferredChatModel,
        providerModels: { ...defaultProviderModels },
        subagents: subagents.map(sa => ({
          id: sa.id,
          model: sa.model,
          specificModel: sa.specificModel,
          active: false
        })),
        conversationModels
      };
      localStorage.setItem(MODEL_PREFS_STORAGE_KEY, JSON.stringify(prefs));
    } catch (e) {
      console.error("Failed to save model preferences:", e);
    }
  }

  function getSubagentRoleKey(sa: Subagent): string | null {
    if (sa.id === "sa-1") return "research";
    if (sa.id === "sa-2b") return "backend";
    if (sa.id === "sa-2f") return "frontend";
    if (sa.id === "sa-3") return "verification";
    return null;
  }

  function normalizeCanonicalAgentKey(value: unknown): AgentKey | null {
    if (typeof value !== "string") return null;
    const key = value.trim().toLowerCase();
    if (key === "research" || key === "backend" || key === "frontend" || key === "verification") {
      return key as AgentKey;
    }
    return null;
  }

  function getSubagentByRoleKey(agentKey: AgentKey): Subagent | null {
    return subagents.find(sa => getSubagentRoleKey(sa) === agentKey) || null;
  }

  function hasDelegatedAgent(parentSessionId: string, agentKey: AgentKey): boolean {
    return !!delegatedAgentKeysBySession[parentSessionId]?.[agentKey];
  }

  function markDelegatedAgent(parentSessionId: string, agentKey: AgentKey) {
    delegatedAgentKeysBySession = {
      ...delegatedAgentKeysBySession,
      [parentSessionId]: {
        ...(delegatedAgentKeysBySession[parentSessionId] || {}),
        [agentKey]: true
      }
    };
  }

  function reserveDelegatedAgents(parentSessionId: string, agentKeys: AgentKey[]): AgentKey[] {
    const reserved: AgentKey[] = [];
    for (const agentKey of agentKeys) {
      if (hasDelegatedAgent(parentSessionId, agentKey)) continue;
      markDelegatedAgent(parentSessionId, agentKey);
      reserved.push(agentKey);
    }
    return reserved;
  }

  function canSessionSpawnSubagents(parentSessionId: string): boolean {
    return parentSessionId === MAIN_SESSION_ID && !!streamingMetaBySession[parentSessionId];
  }

  function getSubagentModelRegistry(): Record<string, BackendSubagentModelSelection> {
    const registry: Record<string, BackendSubagentModelSelection> = {};
    subagents.forEach(sa => {
      const roleKey = getSubagentRoleKey(sa);
      if (!roleKey) return;
      registry[roleKey] = {
        name: sa.name,
        role: sa.role,
        provider: sa.model,
        specificModel: sa.specificModel,
        active: sa.active
      };
    });
    return registry;
  }

  // nTropy Learning Loop Skills
  let skills = $state<Array<{ name: string, description: string, trigger_phrases: string[] }>>([]);
  let activeSkillText = $state("");
  let selectedSkillName = $state("");

  // Approval Request state (Mock transaction cards)
  let pendingApproval = $state<{ id: string, type: string, action: string, path: string } | null>(null);

  // Scheduled Tasks Functions
  async function createNewTask(name: string, command: string, cli: string, schedule: string) {
    if (!currentProjectPath || !activeConversationId) return;
    const newId = `task-${Date.now()}-${Math.random().toString(36).substr(2, 9)}`;
    const newTask: ScheduledTask = {
      id: newId,
      projectPath: currentProjectPath,
      name,
      command,
      cli,
      schedule,
      status: 'active',
      lastRun: null,
      lastResult: null,
      lastOutput: ""
    };
    tasks = [...tasks, newTask];
    
    // Save to SQLite
    try {
      const textJson = JSON.stringify({
        name,
        command,
        cli,
        schedule
      });
      await invoke("create_task", { id: newId, sessionId: activeConversationId, text: textJson });
    } catch (e) {
      console.error("Failed to create task in SQLite:", e);
    }
    
    streamLogs = [...streamLogs, `[TASK RUNNER] Registered task: "${name}"`];
  }

  async function deleteTask(id: string) {
    tasks = tasks.filter(t => t.id !== id);
    try {
      await invoke("delete_task", { id });
    } catch (e) {
      console.error("Failed to delete task in SQLite:", e);
    }
  }

  async function toggleTaskStatus(id: string) {
    let nextStatus: 'active' | 'paused' | 'running' = 'active';
    tasks = tasks.map(t => {
      if (t.id === id) {
        nextStatus = t.status === 'active' ? 'paused' : 'active';
        return { ...t, status: nextStatus };
      }
      return t;
    });
    try {
      await invoke("update_task_status", { id, status: nextStatus });
    } catch (e) {
      console.error("Failed to update task status in SQLite:", e);
    }
  }

  function getIntervalMs(schedule: string): number | null {
    switch (schedule) {
      case "1m": return 60000;
      case "5m": return 300000;
      case "30m": return 1800000;
      case "1h": return 3600000;
      case "1d": return 86400000;
      case "once": return null;
      default: return null;
    }
  }

  async function executeTask(task: ScheduledTask) {
    if (task.status === 'running') return;
    const taskSessionId = `task-session-${task.id}`;
    const taskSpecificModel = getSafeSpecificModel(task.cli);
    registerSessionModel(taskSessionId, task.cli, taskSpecificModel);
    
    // Mark as running and clear output
    tasks = tasks.map(t => t.id === task.id ? { 
      ...t, 
      status: 'running', 
      lastOutput: `[${new Date().toLocaleTimeString()}] Task triggered...\n` 
    } : t);
    
    try {
      await invoke("update_task_status", { id: task.id, status: 'running' });
    } catch (e) {
      console.error("Failed to update task status in SQLite:", e);
    }

    try {
      streamLogs = [...streamLogs, `[TASK RUNNER] Triggered task "${task.name}" using CLI "${task.cli.toUpperCase()}"`];
      
      await invoke("run_cli_prompt", {
        sessionId: taskSessionId,
        runId: createRunId(),
        model: task.cli,
        specificModel: taskSpecificModel,
        prompt: task.command,
        agentMappings: getAgentMappings(),
        providerModels: getProviderModels(),
        subagentModels: getSubagentModelRegistry()
      });
    } catch (e) {
      const finalStatus = task.schedule === 'once' ? 'paused' : 'active';
      tasks = tasks.map(t => t.id === task.id ? { 
        ...t, 
        status: finalStatus,
        lastRun: Date.now(),
        lastResult: 'failed',
        lastOutput: t.lastOutput + `\nExecution Error: ${e}\n` 
      } : t);
      
      try {
        await invoke("update_task_status", { id: task.id, status: finalStatus });
      } catch (err) {
        console.error("Failed to update task status in SQLite:", err);
      }
    }
  }

  onMount(() => {
    let unlistenCliOutput: any = null;
    let unlistenCliFinished: any = null;
    let unlistenSpawnSubagent: any = null;
    let schedulerInterval: any = null;

    async function init() {
      loadModelPreferences();
      if (!canUseTauriIpc()) {
        loadRecentProjects();
        return;
      }

      // 2. Listen for background subprocess streams from Rust
      unlistenCliOutput = await listen("cli-output", (event: any) => {
        const payload: any = event.payload;
        const formattedLog = `[${payload.stream.toUpperCase()}] ${payload.data}`;
        appendStreamLog(formattedLog);
        
        // Route output to task if it belongs to a task run session
        if (payload.session_id.startsWith("task-session-")) {
          recordBackendModelSelection(payload.session_id, payload.data);
          recordTokenUsage(payload.session_id, payload.data);
          const taskId = payload.session_id.replace("task-session-", "");
          const sanitized = sanitizeStdout(payload.data);
          tasks = tasks.map(t => {
            if (t.id === taskId) {
              return {
                ...t,
                lastOutput: t.lastOutput + sanitized
              };
            }
            return t;
          });
          return;
        }
        
        // Stream stdout and stderr to the matching chat bubble for any live CLI session.
        if (streamingMessageBySession[payload.session_id]) {
          appendStreamingChunk(payload.session_id, payload.stream, payload.data);
        }

        // Auto scroll terminal logs & chat scroller
        setTimeout(() => {
          document.querySelectorAll(".terminal-content").forEach((term) => {
            term.scrollTop = term.scrollHeight;
          });

          const chatScroller = document.querySelector(".chat-scroller");
          if (chatScroller) {
            chatScroller.scrollTop = chatScroller.scrollHeight;
          }
        }, 10);
      });

      // 3. Listen for CLI task completion
      unlistenCliFinished = await listen("cli-finished", async (event: any) => {
        const finished = normalizeCliFinishedPayload(event.payload);
        if (!finished) {
          streamLogs = [...streamLogs, `[CLI] Ignored malformed cli-finished payload: ${JSON.stringify(event.payload)}`];
          return;
        }
        const session_id = finished.sessionId;
        if (session_id.startsWith("task-session-")) {
          const taskId = session_id.replace("task-session-", "");
          let finalStatus: 'active' | 'paused' | 'running' = 'active';
          tasks = tasks.map(t => {
            if (t.id === taskId) {
              finalStatus = t.schedule === 'once' ? 'paused' : 'active';
              const resultText = finished.success
                ? "Task finished successfully."
                : `Task ${formatFinishedStatus(finished)}.`;
              return {
                ...t,
                status: finalStatus,
                lastRun: Date.now(),
                lastResult: finished.success ? 'success' : 'failed',
                lastOutput: t.lastOutput + `\n[${new Date().toLocaleTimeString()}] ${resultText}\n`
              };
            }
            return t;
          });
          invoke("update_task_status", { id: taskId, status: finalStatus }).catch(e => {
            console.error("Failed to update task status in SQLite:", e);
          });
          streamLogs = [...streamLogs, `[TASK RUNNER] Task execution ${formatFinishedStatus(finished)}: ${taskId}`];
        } else {
          streamLogs = [...streamLogs, `[CLI] Session ${session_id} ${formatFinishedStatus(finished)}`];
          await finishStreamingSession(session_id, finished.success ? undefined : failureFallbackForFinished(finished));
        }
      });

      // 4. Listen for dynamic subagent spawning commands intercepted in active CLI streams
      unlistenSpawnSubagent = await listen("spawn-subagent", async (event: any) => {
        const payload: any = event.payload;
        const agentKey = normalizeCanonicalAgentKey(payload.agent_key || payload.agentKey || payload.agent);
        if (!agentKey) {
          appendStreamLog(`[SUBAGENT] Rejected spawn: missing exact canonical agent key. Expected one of research, backend, frontend, verification.`);
          return;
        }

        const parentSessionIdRaw = payload.parent_session_id || payload.parentSessionId;
        if (typeof parentSessionIdRaw !== "string" || !parentSessionIdRaw.trim()) {
          appendStreamLog(`[SUBAGENT] Rejected spawn: missing parent session id.`);
          return;
        }
        const parentSessionId = parentSessionIdRaw.trim();
        if (!canSessionSpawnSubagents(parentSessionId)) {
          appendStreamLog(`[SUBAGENT] Ignored nested delegation from ${parentSessionId}; only the main orchestrator can spawn registry agents.`);
          return;
        }

        const parentRunIdRaw = payload.parent_run_id || payload.parentRunId;
        if (typeof parentRunIdRaw !== "string" || !parentRunIdRaw.trim()) {
          appendStreamLog(`[SUBAGENT] Rejected spawn from ${parentSessionId}: missing parent run id.`);
          return;
        }
        const parentRunId = parentRunIdRaw.trim();
        const parentMeta = streamingMetaBySession[parentSessionId];
        if (!parentMeta || parentMeta.runId !== parentRunId) {
          appendStreamLog(`[SUBAGENT] Ignored stale delegation from ${parentSessionId}; run id no longer matches the active prompt.`);
          return;
        }

        const registryAgent = subagents.find(sa => getSubagentRoleKey(sa) === agentKey);
        if (!registryAgent) {
          appendStreamLog(`[SUBAGENT] Rejected spawn: no configured registry entry for canonical agent "${agentKey}".`);
          return;
        }

        if (hasDelegatedAgent(parentSessionId, agentKey)) {
          appendStreamLog(`[SUBAGENT] Ignored duplicate delegation for exact ${agentKey}; that registry agent is already running for this prompt.`);
          return;
        }

        markDelegatedAgent(parentSessionId, agentKey);
        const conversationId = parentMeta?.conversationId || activeConversationId;
        appendStreamLog(`[SUBAGENT] Delegating to exact ${agentKey}: ${registryAgent.model.toUpperCase()} (${registryAgent.specificModel})`);
        const sourcePrompt = promptBySession[parentSessionId] || promptBySession[MAIN_SESSION_ID] || getLatestUserPrompt();
        await launchSubagent(registryAgent, sourcePrompt, payload.task, conversationId || undefined, parentSessionId);
      });

      // 5. Fetch Rules parsed dynamically from C:\Users\User\Desktop\dev-rules\index.html
      try {
        const parsedRules: any = await invoke("get_rules");
        rulesList = parsedRules.non_negotiables;
        // Enabled by default as requested
        rulesList.forEach(r => {
          ruleCheckList[r.id] = true;
        });
      } catch (e) {
        console.error("Failed to load dev-rules manifest:", e);
      }

      // 6. Fetch active project and load files dynamically
      try {
        currentProjectPath = await invoke("get_active_project");
        loadRecentProjects();
        
        if (currentProjectPath) {
          showStartupChooser = false;
          addRecentProject(currentProjectPath);
          // Load files
          files = await invoke("open_project", { path: currentProjectPath });
          
          await loadProjectDatabaseState();
          
          if (files.length > 0) {
            selectedFile = files[0];
            await loadSymbols(selectedFile);
          }
        }
      } catch (e) {
        console.error("Failed to load active project:", e);
        if (currentProjectPath) {
          await refreshFiles();
        }
      }

      // 7. Fetch Level 0 skills index
      await refreshSkills();
    }

    init();

    // 8. Start Background Tasks Scheduler checking every 5 seconds
    schedulerInterval = setInterval(() => {
      const now = Date.now();
      tasks.forEach(async (task) => {
        if (task.status !== 'active') return;
        
        let shouldRun = false;
        if (!task.lastRun) {
          shouldRun = true;
        } else {
          const elapsed = now - task.lastRun;
          const interval = getIntervalMs(task.schedule);
          if (interval && elapsed >= interval) {
            shouldRun = true;
          }
        }

        if (shouldRun) {
          await executeTask(task);
        }
      });
    }, 5000);

    // Return Svelte cleanup triggers
    return () => {
      if (unlistenCliOutput) unlistenCliOutput();
      if (unlistenCliFinished) unlistenCliFinished();
      if (unlistenSpawnSubagent) unlistenSpawnSubagent();
      if (schedulerInterval) clearInterval(schedulerInterval);
    };
  });

  async function refreshSkills() {
    try {
      skills = await invoke("get_skills_index");
    } catch (e) {
      console.error("Failed to refresh nTropy skills:", e);
    }
  }

  async function viewSkill(name: string) {
    selectedSkillName = name;
    try {
      activeSkillText = await invoke("get_skill_text", { name });
    } catch (e) {
      activeSkillText = "Failed to load skill details.";
    }
  }

  async function loadSymbols(filePath: string) {
    selectedFile = filePath;
    try {
      const res: any = await invoke("index_symbols", { relativePath: filePath });
      fileSymbols = res.symbols;
    } catch (e) {
      fileSymbols = [];
    }
  }

  function getLatestUserPrompt(): string {
    const latestUserMessage = chatMessages.slice().reverse().find(m => m.type === "user");
    return latestUserMessage ? latestUserMessage.text : "";
  }

  const canonicalAgentOrder: AgentKey[] = ["research", "backend", "frontend", "verification"];

  function hasWordLike(text: string, pattern: RegExp): boolean {
    return pattern.test(text);
  }

  function looksLikeWorkPrompt(promptText: string): boolean {
    const lower = promptText.toLowerCase();
    const trimmed = lower.trim();
    if (!trimmed) return false;

    if (
      trimmed.includes("```") ||
      trimmed.includes("[stdout]") ||
      trimmed.includes("[stderr]") ||
      trimmed.includes("[orchestrator]") ||
      trimmed.includes("[rules gate]") ||
      trimmed.includes("c:\\") ||
      /\.(rs|svelte|ts|js|json|toml|md|html|css|mjs|cjs)\b/.test(trimmed) ||
      /\b(npm|cargo|git|powershell|cmd|node|tauri)\b/.test(trimmed)
    ) {
      return true;
    }

    return hasWordLike(
      trimmed,
      /\b(agent|analyze|app|auth|backend|build|check|cli|code|component|create|debug|delegate|deploy|edit|error|file|fix|folder|frontend|implement|inspect|model|orchestrator|project|read|refactor|research|review|route|run|search|subagent|tauri|test|update|validate|verify|write)\b/
    );
  }

  function findRoutedSubagentForPrompt(promptText: string): Subagent | null {
    const lower = promptText.toLowerCase();
    if (lower.includes("research agent") || lower.includes("researcher")) {
      return subagents.find(sa => sa.id === "sa-1") || null;
    }
    if (lower.includes("backend coder agent") || lower.includes("backend coder")) {
      return subagents.find(sa => sa.id === "sa-2b") || null;
    }
    if (lower.includes("frontend coder agent") || lower.includes("frontend coder")) {
      return subagents.find(sa => sa.id === "sa-2f") || null;
    }
    if (lower.includes("verification agent") || lower.includes("verifier")) {
      return subagents.find(sa => sa.id === "sa-3") || null;
    }
    return null;
  }

  function getRequestedAgentKeys(promptText: string): AgentKey[] {
    const lower = promptText.toLowerCase();
    const nonCasualWork = looksLikeWorkPrompt(promptText);
    const wantsAgentDelegation = /\b(subagents?|agents?|delegate|delegation|spawn)\b/.test(lower);
    const requested = new Set<AgentKey>();

    if (!nonCasualWork && !wantsAgentDelegation) {
      return [];
    }

    if (/\ball\b|\bevery\b|\bfull\b|\bcomplete\b/.test(lower) && wantsAgentDelegation) {
      return canonicalAgentOrder;
    }

    if (nonCasualWork) {
      requested.add("research");
    }

    const creationOrBuild = /\b(make|build|create|implement|add|write|generate|scaffold|app|site|game|feature|component|page|ui)\b/.test(lower);
    const codeChange = /\b(fix|debug|repair|change|update|edit|refactor|wire|integrate|auth|routing|orchestrator|model|cli|database|db|state|storage|persistence)\b/.test(lower);
    const frontendWork = /\b(frontend|front end|ui|ux|svelte|typescript|css|html|style|layout|component|page|screen|browser|app|site|game)\b/.test(lower);
    const backendWork = /\b(backend|back end|server|api|rust|tauri|command|filesystem|database|db|sqlite|auth|cli|orchestrator|model|routing|state|storage|persistence|process)\b/.test(lower);
    const verificationWork = /\b(verification|validation|verify|validate|tests?|checks?|smoke|build|cargo|npm|error|bug|debug|fix|regression)\b/.test(lower);
    const analysisOnly = /\b(research|inspect|read|analyze|analysis|review|investigate|look at|figure out|what happened|why)\b/.test(lower);

    if (/\bresearch\b|\bresearcher\b|\binspect\b|\bcodebase\b|\bread\b|\banalyze\b|\breview\b|\binvestigate\b/.test(lower)) {
      requested.add("research");
    }
    if (creationOrBuild || backendWork || codeChange) {
      requested.add("backend");
    }
    if (creationOrBuild || frontendWork) {
      requested.add("frontend");
    }
    if (creationOrBuild || verificationWork || codeChange) {
      requested.add("verification");
    }

    if (analysisOnly && requested.size === 1) {
      requested.add("verification");
    }

    if (requested.size === 0 && (nonCasualWork || wantsAgentDelegation)) {
      requested.add("research");
    }

    return canonicalAgentOrder.filter(key => requested.has(key));
  }

  function getDelegatedTask(agentKey: AgentKey): string {
    if (agentKey === "research") {
      return "Inspect the existing project and identify relevant files, architecture, constraints, and risks for the requested build.";
    }
    if (agentKey === "backend") {
      return "Handle the backend, filesystem, Rust/Tauri, command execution, and data-flow work required by the requested build.";
    }
    if (agentKey === "frontend") {
      return "Handle the frontend, Svelte, TypeScript, styling, and user-facing behavior required by the requested build.";
    }
    return "Run validation for the requested build, including relevant checks/tests, and report concrete failures or pass status.";
  }

  async function launchRequestedSubagents(
    parentSessionId: string,
    sourcePrompt: string,
    reservedAgentKeys?: AgentKey[],
    conversationId = activeConversationId
  ) {
    const requestedAgentKeys = reservedAgentKeys || reserveDelegatedAgents(parentSessionId, getRequestedAgentKeys(sourcePrompt));
    if (requestedAgentKeys.length === 0 || !conversationId) return;

    appendStreamLog(`[ORCHESTRATOR] Mandatory delegation: ${requestedAgentKeys.join(", ")}`);

    for (const agentKey of requestedAgentKeys) {
      const agent = getSubagentByRoleKey(agentKey);
      if (!agent) {
        appendStreamLog(`[SUBAGENT] Cannot delegate ${agentKey}: no configured registry entry.`);
        continue;
      }

      appendStreamLog(`[SUBAGENT] Delegating to exact ${agentKey}: ${agent.model.toUpperCase()} (${agent.specificModel})`);
      await launchSubagent(agent, sourcePrompt, getDelegatedTask(agentKey), conversationId, parentSessionId);
    }
  }

  function getProviderModelsForPrompt(promptText: string): Record<string, string> {
    const models: Record<string, string> = { ...defaultProviderModels };
    const routedSubagent = findRoutedSubagentForPrompt(promptText);
    if (routedSubagent) {
      models[routedSubagent.model] = routedSubagent.specificModel;
    }
    return models;
  }

  function getProviderModelsForSubagent(sa: Subagent): Record<string, string> {
    return {
      ...defaultProviderModels,
      [sa.model]: sa.specificModel
    };
  }

  function buildSubagentPrompt(sa: Subagent, sourcePrompt: string, requestedTask?: string): string {
    const taskLine = requestedTask && requestedTask.trim()
      ? requestedTask.trim()
      : `Help complete the original user request from your role: ${sa.role}.`;

    return [
      `You are ${sa.name}, an nTropy companion subagent.`,
      `Assigned role: ${sa.role}.`,
      `Specific task: ${taskLine}`,
      "",
      "Original user request:",
      sourcePrompt,
      "",
      "Work only on your assigned slice. You are a worker, not the coordinator.",
      "Do not delegate, do not spawn more subagents, and never print SPAWN_SUBAGENT.",
      "Return concise, actionable output for the shared chat."
    ].join("\n");
  }

  async function launchSubagent(sa: Subagent, sourcePrompt: string, requestedTask?: string, conversationId = activeConversationId, parentSessionId?: string) {
    if (!conversationId) return;
    if (hasLiveSubagentSession(sa.id)) {
      const roleKey = getSubagentRoleKey(sa) || sa.id;
      appendStreamLog(`[SUBAGENT] Ignored duplicate launch for ${roleKey}; that subagent is already running.`);
      return;
    }
    const cleanSourcePrompt = sourcePrompt.trim();
    if (!cleanSourcePrompt) {
      await addMessageToConversation(conversationId, "Orchestrator", `No prompt is available for ${sa.name} yet. Send a chat prompt first, or type one in the input before running a subagent.`, 'system');
      return;
    }

    const runSuffix = `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    const sessionId = `subagent-session-${sa.id}-${runSuffix}`;
    const streamMsgId = `subagent-stream-${sa.id}-${runSuffix}`;
    const subagentPrompt = buildSubagentPrompt(sa, cleanSourcePrompt, requestedTask);
    const sender = `${sa.name} Subagent`;

    setSubagentActive(sa.id, true);
    await addMessageToConversation(conversationId, sender, "", 'agent', streamMsgId, false);
    const runId = registerStreamingSession(sessionId, streamMsgId, subagentPrompt, sa.model, sa.specificModel, conversationId, sender, 'agent', sa.id, parentSessionId);

    try {
      await invoke("run_cli_prompt", {
        sessionId,
        runId,
        model: sa.model,
        specificModel: sa.specificModel,
        prompt: subagentPrompt,
        agentMappings: getAgentMappings(),
        providerModels: getProviderModelsForSubagent(sa),
        subagentModels: getSubagentModelRegistry()
      });
    } catch (e) {
      updateStreamingMessageText(streamMsgId, `CLI Fail: ${e}`, conversationId);
      await finishStreamingSession(sessionId, `CLI Fail: ${e}`);
    }
  }

  async function runSubagentFromLatestUserMessage(sa: Subagent) {
    const sourcePrompt = prompt.trim() || getLatestUserPrompt();
    await launchSubagent(sa, sourcePrompt);
  }

  async function sendPrompt() {
    if (!prompt.trim() || !activeConversationId || activeStreamingMessageId) return;
    
    const conversationId = activeConversationId;
    const currentPrompt = prompt;
    prompt = "";

    // Add User Message
    await addMessageToConversation(conversationId, "User", currentPrompt, 'user');

    // Highlight any parsed dynamic rule triggers
    const triggerViolations: any = await invoke("check_rules_action", { 
      trigger: "before-edit", 
      content: currentPrompt 
    });
    
    if (triggerViolations.length > 0) {
      streamLogs = [...streamLogs, `[RULES GATE] Triggered rule constraint - ${triggerViolations[0].text}`];
    }

    // Allocate streaming chat bubble for the agent response
    const runSuffix = `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    const streamMsgId = `agent-stream-${runSuffix}`;
    const activeProvider = activeModel;
    const activeSpecificModel = getSafeSpecificModel(activeProvider);
    const sender = `${activeProvider.toUpperCase()} Agent`;
    await addMessageToConversation(conversationId, sender, "", 'agent', streamMsgId, false);
    const runId = registerStreamingSession(MAIN_SESSION_ID, streamMsgId, currentPrompt, activeProvider, activeSpecificModel, conversationId, sender);
    const deterministicAgentKeys = reserveDelegatedAgents(MAIN_SESSION_ID, getRequestedAgentKeys(currentPrompt));

    // Auto-scroll after adding user message & placeholder
    setTimeout(() => {
      const chatScroller = document.querySelector(".chat-scroller");
      if (chatScroller) {
        chatScroller.scrollTop = chatScroller.scrollHeight;
      }
    }, 10);

    try {
      // Execute global CLI subprocess
      await invoke("run_cli_prompt", {
        sessionId: MAIN_SESSION_ID,
        runId,
        model: activeProvider,
        specificModel: activeSpecificModel,
        prompt: currentPrompt,
        agentMappings: getAgentMappings(),
        providerModels: getProviderModelsForPrompt(currentPrompt),
        subagentModels: getSubagentModelRegistry()
      });
      await launchRequestedSubagents(MAIN_SESSION_ID, currentPrompt, deterministicAgentKeys, conversationId);
    } catch (e) {
      updateStreamingMessageText(streamMsgId, `CLI Fail: ${e}`, conversationId);
      await finishStreamingSession(MAIN_SESSION_ID, `CLI Fail: ${e}`);
    }
  }

  function handleModelSwitch(model: string) {
    if (!isKnownProvider(model)) return;
    preferredChatModel = model;
    if (activeConversationId) {
      storedConversationModels = { ...storedConversationModels, [activeConversationId]: model };
      conversations = conversations.map(c => {
        if (c.id === activeConversationId) {
          return {
            ...c,
            activeModel: model
          };
        }
        return c;
      });
    }
    saveModelPreferences();
  }

  function handleUnderlyingModelSwitch(provider: string, specificModel: string) {
    if (!isKnownProvider(provider) || !isValidProviderModel(provider, specificModel)) return;
    defaultProviderModels = {
      ...defaultProviderModels,
      [provider]: specificModel
    };
    saveModelPreferences();
  }

  function toggleSubagentModel(id: string, model: string) {
    if (!isKnownProvider(model)) return;
    subagents = subagents.map(sa => {
      if (sa.id === id) {
        const defaultSpecific = getSafeSpecificModel(model);
        return { ...sa, model, specificModel: defaultSpecific };
      }
      return sa;
    });
    saveModelPreferences();
  }

  function toggleSubagentSpecificModel(id: string, specificModel: string) {
    subagents = subagents.map(sa => {
      if (sa.id === id) {
        if (!isValidProviderModel(sa.model, specificModel)) return sa;
        return { ...sa, specificModel };
      }
      return sa;
    });
    saveModelPreferences();
  }

  function getProviderModels(): Record<string, string> {
    return { ...defaultProviderModels };
  }

  function getAgentMappings(): Record<string, string> {
    const mappings: Record<string, string> = {};
    subagents.forEach(sa => {
      if (sa.id === "sa-1") mappings["research"] = sa.model;
      else if (sa.id === "sa-2b") mappings["backend"] = sa.model;
      else if (sa.id === "sa-2f") mappings["frontend"] = sa.model;
      else if (sa.id === "sa-3") mappings["verification"] = sa.model;
    });
    return mappings;
  }

  function approveTransaction(approved: boolean) {
    if (pendingApproval) {
      addMessageToActiveConversation("Security Kernel", approved ? `Transaction APPROVED for: ${pendingApproval.action}` : `Transaction REJECTED for: ${pendingApproval.action}`, 'system');
      pendingApproval = null;
    }
  }
</script>

<svelte:window onclick={() => showContextMenu = false} oncontextmenu={() => showContextMenu = false} />

<main class="app-frame">
  <!-- nTropy App Header (Menu Bar) -->
  <div class="menu-bar">
    {#if !showSidebar}
      <button 
        class="sidebar-toggle-trigger-btn" 
        onclick={() => showSidebar = true} 
        title="Show Sidebar" 
        style="background: transparent; border: none; color: var(--text-muted); cursor: pointer; display: inline-flex; align-items: center; justify-content: center; padding: 0.2rem; margin-right: 0.5rem; border-radius: 4px; transition: color 0.15s ease;"
        onmouseenter={(e: any) => e.target.style.color = 'var(--text-primary)'}
        onmouseleave={(e: any) => e.target.style.color = 'var(--text-muted)'}
      >
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" style="width: 15px; height: 15px;">
          <rect width="18" height="18" x="3" y="3" rx="2" />
          <path d="M9 3v18" />
        </svg>
      </button>
    {/if}
    <span class="menu-item active-app">nTropy</span>
    <span class="menu-item">File</span>
    <span class="menu-item">View</span>
    <span class="menu-item">Window</span>
    
    <!-- OAuth Status Checks (Subtle, Muted Colors) -->
    <div class="oauth-status-panel">
      <div class="status-pill">
        <span class="dot green"></span>
        <span class="pill-label">CC</span>
      </div>
      <div class="status-pill">
        <span class="dot green"></span>
        <span class="pill-label">Gemini</span>
      </div>
      <div class="status-pill">
        <span class="dot green"></span>
        <span class="pill-label">Grok</span>
      </div>
      <div class="status-pill">
        <span class="dot green"></span>
        <span class="pill-label">DZL</span>
      </div>
    </div>

    <!-- Toggle Inspector Tabs Button -->
    <button class="toggle-tabs-btn" onclick={() => showRightPanel = !showRightPanel} class:active={showRightPanel}>
      {showRightPanel ? 'Hide Tabs' : 'Show Tabs'}
    </button>
  </div>

  <!-- Central Layout Grid -->
  <div class="dashboard-grid">
    
    <!-- LEFT PANEL: Unified nTropy-style Sidebar -->
    <aside class="side-panel left-panel" class:collapsed={!showSidebar}>
      <!-- Sidebar Control Bar (Sidebar layout) -->
      <div class="sidebar-top-bar">
        <button class="sidebar-tool-btn toggle-sidebar-btn" onclick={() => showSidebar = false} title="Collapse Sidebar">
          <svg class="sidebar-toggle-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" style="width: 15px; height: 15px;">
            <rect width="18" height="18" x="3" y="3" rx="2" />
            <path d="M9 3v18" />
          </svg>
        </button>
      </div>
      
      <!-- Core Nav Actions -->
      <div class="sidebar-nav">
        <button class="nav-btn" class:active={currentView === 'tasks'} onclick={() => currentView = 'tasks'}>
          <span class="icon" style="display: inline-flex; align-items: center;">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" style="width: 14px; height: 14px;">
              <rect x="3" y="4" width="18" height="18" rx="2" ry="2" />
              <line x1="16" y1="2" x2="16" y2="6" />
              <line x1="8" y1="2" x2="8" y2="6" />
              <line x1="3" y1="10" x2="21" y2="10" />
              <circle cx="12" cy="16" r="3" />
              <polyline points="12 15 12 16 13 16" />
            </svg>
          </span>
          Scheduled Tasks
        </button>
      </div>

      <!-- Projects Section -->
      <div class="sidebar-section">
        <div class="sidebar-section-header">
          <span class="section-title">Projects</span>
          <div class="section-actions">
            <span class="filter-icon" title="Filter Projects">⧛</span>
            <button class="add-project-icon-btn" onclick={handleChoosePathDirect} title="Open / Add Project Folder">
              <svg class="folder-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" style="width: 15px; height: 15px; vertical-align: -2px;">
                <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"></path>
                <line x1="12" y1="11" x2="12" y2="17"></line>
                <line x1="9" y1="14" x2="15" y2="14"></line>
              </svg>
            </button>
          </div>
        </div>
        <div class="projects-list">
          {#if recentProjects.length === 0}
            <div class="sidebar-empty">No projects added yet.</div>
          {:else}
            {#each recentProjects as projectPath}
              {@const projName = projectPath.split('\\').pop() || projectPath.split('/').pop() || projectPath}
              {@const isActive = currentProjectPath === projectPath}
              <div class="project-folder-block" class:active={isActive}>
                <div class="project-folder-row" style="display: flex; align-items: center; justify-content: space-between; width: 100%;">
                  <button class="project-folder-btn" onclick={() => handleOpenProject(projectPath)} style="flex: 1; min-width: 0; width: auto;">
                    <span class="folder-icon" style="display: inline-flex; align-items: center;">
                      <svg class="folder-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">
                        <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"></path>
                        {#if isActive}
                          <circle cx="12" cy="13" r="2.5" fill="currentColor"></circle>
                        {/if}
                      </svg>
                    </span>
                    <span class="folder-name">{projName}</span>
                  </button>
                  <button class="add-conv-btn" onclick={() => { createNewConversation(projectPath); currentView = "chat"; }} title="New Chat in this Project" style="background: transparent; border: none; color: var(--text-muted); cursor: pointer; padding: 0.2rem 0.4rem; font-size: 0.95rem; font-weight: 500; display: flex; align-items: center; justify-content: center; transition: color 0.15s ease; border-radius: 4px;" onmouseenter={(e: any) => e.target.style.color = 'var(--text-primary)'} onmouseleave={(e: any) => e.target.style.color = 'var(--text-muted)'}>
                    ＋
                  </button>
                </div>
                <div class="project-conversation-links" style="display: flex; flex-direction: column; gap: 0.15rem; padding-left: 1.25rem; margin-top: 0.2rem;">
                  {#each conversations.filter(c => c.projectPath === projectPath) as conv}
                    {@const isConvActive = activeConversationId === conv.id}
                    <button 
                      class="conv-btn" 
                      class:active={isConvActive} 
                      onclick={() => { 
                        currentProjectPath = projectPath; 
                        activeConversationId = conv.id; 
                        currentView = "chat"; 
                      }}
                      oncontextmenu={(e) => handleContextMenu(e, conv.id)}
                    >
                      <span class="conv-preview" style="max-width: 140px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;">↳ {conv.title}</span>
                      <span class="conv-time" style="font-size: 0.7rem; color: var(--text-muted); margin-left: auto;">{getConvTimeText(conv.lastUpdated)}</span>
                    </button>
                  {/each}
                </div>
              </div>
            {/each}
          {/if}
        </div>
      </div>

      <!-- Conversations Section (Historical Chats from other projects) -->
      <div class="sidebar-section">
        <div class="sidebar-section-header">
          <span class="section-title">Conversations</span>
        </div>
        <div class="conversations-list">
          {#each conversations.filter(c => c.projectPath !== currentProjectPath) as conv}
            <button 
              class="sidebar-conv-btn" 
              onclick={() => { 
                currentProjectPath = conv.projectPath; 
                activeConversationId = conv.id; 
                currentView = "chat"; 
              }}
              oncontextmenu={(e) => handleContextMenu(e, conv.id)}
            >
              <span class="conv-preview" style="max-width: 160px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;">{conv.title}</span>
              <span class="conv-time" style="font-size: 0.7rem; color: var(--text-muted); margin-left: auto;">{getConvTimeText(conv.lastUpdated)}</span>
            </button>
          {:else}
            <div class="sidebar-empty">No other conversations.</div>
          {/each}
        </div>
      </div>

      <!-- Collapsible Active File Explorer -->
      <div class="sidebar-section collapsible-section">
        <button class="section-toggle-btn" onclick={() => showFilesSection = !showFilesSection}>
          <span class="section-title">Active Files {showFilesSection ? '▼' : '▶'}</span>
        </button>
        
        {#if showFilesSection}
          {#if !currentProjectPath}
            <div class="sidebar-empty">No active project.</div>
          {:else if files.length === 0}
            <div class="sidebar-empty">No files found.</div>
          {:else}
            <div class="compact-file-list">
              {#each files as file}
                <button 
                  class="compact-file-item" 
                  class:selected={selectedFile === file}
                  onclick={() => loadSymbols(file)}
                >
                  <span class="icon">📄</span>
                  <span class="name">{file.split('/').pop()}</span>
                </button>
              {/each}
            </div>
          {/if}
        {/if}
      </div>
    </aside>

    <!-- CENTER PANEL: Unified Chat & Interactive CLI terminal logs -->
    <section class="main-panel">
      {#if currentView === 'chat'}
        <!-- Global Switching Bar -->
        <div class="model-selection-bar">
          <span class="model-bar-title">Active Chat LLM:</span>
          <div class="model-select-wrapper">
            <select class="model-select-dropdown" value={activeModel} onchange={(e: any) => handleModelSwitch(e.target.value)}>
              <option value="claude">Claude Code CLI</option>
              <option value="gemini">Gemini CLI</option>
              <option value="grok">Grok CLI</option>
              <option value="codex">Codex CLI</option>
            </select>
            <span class="select-arrow">▼</span>
          </div>

          <!-- Secondary Specific Model Selector -->
          <span class="model-bar-title" style="margin-left: 1.2rem;">Underlying Model:</span>
          <div class="model-select-wrapper">
            <select 
              class="model-select-dropdown" 
              value={defaultProviderModels[activeModel]} 
              onchange={(e: any) => handleUnderlyingModelSwitch(activeModel, e.target.value)}
            >
              {#each providerModelsMap[activeModel] || [] as specificOption}
                <option value={specificOption}>{formatSpecificModelLabel(specificOption)}</option>
              {/each}
            </select>
            <span class="select-arrow">▼</span>
          </div>
        </div>

        <!-- Unified Chat Pane -->
        <div class="chat-container">
          <div class="chat-scroller">
            {#each chatMessages as msg}
              <div class="message-card" class:user={msg.type === 'user'} class:system={msg.type === 'system'} class:agent={msg.type === 'agent'}>
                <div class="msg-sender">{msg.sender}</div>
                <div class="msg-text">
                  {msg.text}
                  {#if msg.id === activeStreamingMessageId}
                    <span class="blinking-cursor">▊</span>
                  {/if}
                </div>
              </div>
            {/each}

            <!-- Proof of Life active thinking indicator -->
            {#if activeStreamingMessageId}
              <div class="processing-loader-card">
                <div class="loader-header">
                  <span class="pulse-dot"></span>
                  <span class="loader-title">ORCHESTRATOR ACTIVE PROCESSING THREAD</span>
                </div>
                <p class="loader-subtitle">Executing direct system execution through {activeModel.toUpperCase()} CLI...</p>
                <div class="progress-bar-container">
                  <div class="scanning-laser"></div>
                </div>
              </div>
            {/if}

            <!-- Secure transaction card if pending -->
            {#if pendingApproval}
              <div class="transaction-card">
                <h4 class="card-title">⚠️ Secure Intercept: Approval Required</h4>
                <p class="card-body">The active subagent requests permission to execute:</p>
                <div class="card-code"><code>{pendingApproval.action}</code></div>
                <div class="card-actions">
                  <button class="approve-btn" onclick={() => approveTransaction(true)}>Approve Execution</button>
                  <button class="deny-btn" onclick={() => approveTransaction(false)}>Deny</button>
                </div>
              </div>
            {/if}
          </div>

          <!-- Chat Prompt input -->
          <form class="input-form" onsubmit={(e) => { e.preventDefault(); sendPrompt(); }}>
            <input 
              type="text" 
              placeholder={activeStreamingMessageId ? "[Agent is actively executing. Thread locked...]" : "Type prompt here... (e.g. check index symbols, save skill, run build)"}
              bind:value={prompt}
              disabled={!!activeStreamingMessageId}
            />
            <button type="submit" disabled={!!activeStreamingMessageId}>
              {activeStreamingMessageId ? "Executing..." : "Execute Action"}
            </button>
          </form>
        </div>
      {:else if currentView === 'tasks'}
        <div class="tasks-dashboard">
          <div class="dashboard-header">
            <h2>🕒 Scheduled Task Manager</h2>
            <p>Orchestrate automated CLI scripts in the background and assign customized LLMs.</p>
          </div>

          <!-- Add New Task Form -->
          <div class="task-form-card">
            <h3>Create Automated CLI Task</h3>
            <form onsubmit={(e) => {
              e.preventDefault();
              const form = e.target as HTMLFormElement;
              const name = (form.elements.namedItem("taskName") as HTMLInputElement).value;
              const command = (form.elements.namedItem("taskCommand") as HTMLInputElement).value;
              const cli = (form.elements.namedItem("taskCli") as HTMLSelectElement).value;
              const schedule = (form.elements.namedItem("taskSchedule") as HTMLSelectElement).value;
              if (name && command) {
                createNewTask(name, command, cli, schedule);
                form.reset();
              }
            }} class="task-creation-form">
              <div class="form-group">
                <label for="taskName">Task Name</label>
                <input type="text" id="taskName" name="taskName" placeholder="e.g. Daily Build Check, Git Sync" required />
              </div>
              
              <div class="form-group">
                <label for="taskCommand">Prompt / CLI Script Command</label>
                <input type="text" id="taskCommand" name="taskCommand" placeholder="e.g. check compilation status, run lint" required />
              </div>

              <div class="form-row">
                <div class="form-group half">
                  <label for="taskCli">Orchestrator CLI</label>
                  <select id="taskCli" name="taskCli">
                    <option value="claude">Claude Code CLI</option>
                    <option value="gemini">Gemini CLI</option>
                    <option value="grok">Grok CLI</option>
                    <option value="codex">Codex CLI</option>
                  </select>
                </div>

                <div class="form-group half">
                  <label for="taskSchedule">Execution Interval</label>
                  <select id="taskSchedule" name="taskSchedule">
                    <option value="once">One-shot (Run Once)</option>
                    <option value="1m">Every 1 Minute</option>
                    <option value="5m">Every 5 Minutes</option>
                    <option value="30m">Every 30 Minutes</option>
                    <option value="1h">Every 1 Hour</option>
                    <option value="1d">Daily (24 Hours)</option>
                  </select>
                </div>
              </div>

              <button type="submit" class="submit-task-btn">Add Automated Task</button>
            </form>
          </div>

          <!-- Existing Tasks List -->
          <div class="tasks-list-container">
            <h3>Registered Project Tasks ({tasks.filter(t => t.projectPath === currentProjectPath).length})</h3>
            {#if tasks.filter(t => t.projectPath === currentProjectPath).length === 0}
              <div class="tasks-empty-state">
                <span class="empty-icon">🕒</span>
                <p>No background tasks registered for the active project.</p>
              </div>
            {:else}
              <div class="tasks-grid">
                {#each tasks.filter(t => t.projectPath === currentProjectPath) as task}
                  <div class="task-card" class:running={task.status === 'running'}>
                    <div class="task-card-header">
                      <span class="task-status-indicator" class:active={task.status === 'active'} class:paused={task.status === 'paused'} class:running={task.status === 'running'}></span>
                      <h4 class="task-name">{task.name}</h4>
                      <span class="task-cli-badge">{task.cli}</span>
                    </div>

                    <div class="task-details">
                      <div class="detail-row">
                        <span class="detail-label">Command:</span>
                        <code class="detail-value">{task.command}</code>
                      </div>
                      <div class="detail-row">
                        <span class="detail-label">Interval:</span>
                        <span class="detail-value">{task.schedule === 'once' ? 'One-shot' : `Every ${task.schedule}`}</span>
                      </div>
                      <div class="detail-row">
                        <span class="detail-label">Last Execution:</span>
                        <span class="detail-value">{task.lastRun ? new Date(task.lastRun).toLocaleString() : 'Never'}</span>
                      </div>
                      <div class="detail-row">
                        <span class="detail-label">Status:</span>
                        <span class="detail-value status-text" class:success={task.lastResult === 'success'} class:failed={task.lastResult === 'failed'}>
                          {task.status.toUpperCase()} ({task.lastResult || 'PENDING'})
                        </span>
                      </div>
                    </div>

                    <div class="task-actions-row">
                      <button class="action-btn run-btn" onclick={() => executeTask(task)} disabled={task.status === 'running'}>
                        {task.status === 'running' ? 'Running...' : 'Run Now'}
                      </button>
                      <button class="action-btn pause-btn" onclick={() => toggleTaskStatus(task.id)}>
                        {task.status === 'paused' ? 'Resume' : 'Pause'}
                      </button>
                      <button class="action-btn delete-btn" onclick={() => deleteTask(task.id)}>
                        Delete
                      </button>
                    </div>

                    <!-- Collapsible Task Output Drawers -->
                    {#if task.lastOutput}
                      <div class="task-output-section">
                        <div class="output-header">Last Output Log</div>
                        <pre class="task-output-log">{task.lastOutput}</pre>
                      </div>
                    {/if}
                  </div>
                {/each}
              </div>
            {/if}
          </div>
        </div>
      {/if}
      <div class="runtime-dock">
        <div class="runtime-usage-strip">
          <span class="runtime-label">Tokens</span>
          {#each Object.values(tokenUsageByModel) as usage}
            <span class="runtime-usage-chip">
              <strong>{usage.provider.toUpperCase()}</strong>
              <span>{usage.specificModel}</span>
              <span>{formatTokens(usage.totalTokens)}</span>
            </span>
          {:else}
            <span class="runtime-empty">No token reports yet.</span>
          {/each}
        </div>
        <div class="terminal-panel inline-terminal">
          <div class="terminal-header">
            <span>Terminal Stdout Log</span>
            <div class="terminal-header-actions">
              <span class="term-status">ACTIVE WRAPPER</span>
              <button class="terminal-action-btn" type="button" onclick={() => streamLogs = []}>Clear</button>
            </div>
          </div>
          <div id="terminal-screen" class="terminal-content">
            {#each streamLogs as log}
              <div class="log-line">{log}</div>
            {/each}
          </div>
        </div>
      </div>
    </section>

    {#if showRightPanel}
      <!-- RIGHT PANEL: Subagents controller & nTropy skills list -->
      <aside class="side-panel right-panel">
        <!-- Tabs for toggling between Subagents, Skills, and Rules -->
        <div class="tab-bar">
          <button class="tab-btn" class:active={activeTab === 'chat'} onclick={() => activeTab = 'chat'}>Subagents</button>
          <button class="tab-btn" class:active={activeTab === 'skills'} onclick={() => activeTab = 'skills'}>nTropy Skills</button>
          <button class="tab-btn" class:active={activeTab === 'rules'} onclick={() => activeTab = 'rules'}>v5.1 Rules</button>
        </div>

        <div class="tab-content-wrapper">
          {#if activeTab === 'chat'}
            <div class="subagent-list">
              <div class="panel-header">
                <h3>Active Subagent Registry</h3>
              </div>
              <div class="usage-panel">
                <div class="usage-title">Tokens Used During Actions</div>
                {#each Object.values(tokenUsageByModel) as usage}
                  <div class="usage-row">
                    <div>
                      <div class="usage-model">{usage.provider.toUpperCase()} · {usage.specificModel}</div>
                      <div class="usage-meta">{usage.actions} action{usage.actions === 1 ? '' : 's'} · last {formatTokens(usage.lastTokens)}</div>
                    </div>
                    <div class="usage-total">{formatTokens(usage.totalTokens)}</div>
                  </div>
                {:else}
                  <div class="usage-empty">No token reports yet.</div>
                {/each}
              </div>
              {#each subagents as sa}
                <div class="subagent-card" class:active={sa.active}>
                  <div class="card-header">
                    <span class="sa-name">{sa.name}</span>
                    <span class="sa-status" class:active={sa.active}>{sa.active ? 'ACTIVE' : 'IDLE'}</span>
                  </div>
                  <div class="sa-role">{sa.role}</div>
                  
                  <!-- Allocation controls -->
                  <div class="sa-controls" style="display: flex; flex-direction: column; align-items: stretch; gap: 0.35rem;">
                    <div style="display: flex; align-items: center; justify-content: space-between; gap: 0.45rem;">
                      <span class="lbl" style="min-width: 32px;">LLM:</span>
                      <select value={sa.model} onchange={(e: any) => toggleSubagentModel(sa.id, e.target.value)} style="flex: 1;">
                        <option value="claude">Claude CLI</option>
                        <option value="gemini">Gemini</option>
                        <option value="grok">Grok CLI</option>
                        <option value="codex">Codex</option>
                      </select>
                    </div>
                    <div style="display: flex; align-items: center; justify-content: space-between; gap: 0.45rem;">
                      <span class="lbl" style="min-width: 32px;">Model:</span>
                      <select value={sa.specificModel} onchange={(e: any) => toggleSubagentSpecificModel(sa.id, e.target.value)} style="flex: 1;">
                        {#each providerModelsMap[sa.model] || [] as specificOption}
                          <option value={specificOption}>{formatSpecificModelLabel(specificOption)}</option>
                        {/each}
                      </select>
                    </div>
                    <button class="sa-run-btn" onclick={() => runSubagentFromLatestUserMessage(sa)}>
                      Run on Current Prompt
                    </button>
                  </div>
                </div>
              {/each}
            </div>
          {:else if activeTab === 'skills'}
            <div class="skills-panel">
              <div class="panel-header">
                <h3>DISTILLED PROCEDURAL MEMORY</h3>
              </div>
              <div class="skills-index">
                {#each skills as s}
                  <button class="skill-chip" onclick={() => viewSkill(s.name)}>
                    <span class="chip-title">{s.name}</span>
                    <span class="chip-desc">{s.description}</span>
                  </button>
                {/each}
              </div>
              {#if activeSkillText}
                <div class="skill-detail">
                  <h4>Skill details: {selectedSkillName}</h4>
                  <pre>{activeSkillText}</pre>
                </div>
              {/if}
            </div>
          {:else if activeTab === 'rules'}
            <div class="rules-view-panel">
              <div class="panel-header">
                <h3>Dynamic Dev-Rules Checked</h3>
              </div>
              <div class="rules-checklist">
                {#each rulesList as r}
                  <label class="rule-label">
                    <input type="checkbox" bind:checked={ruleCheckList[r.id]} />
                    <span class="rule-text" class:blocking={r.severity === 'blocking'}>
                      <strong>[{r.id}]</strong> {r.text}
                    </span>
                  </label>
                {/each}
              </div>
            </div>
          {/if}
        </div>
      </aside>
    {/if}

  </div>
  <!-- Clean Startup Project Chooser Modal Overlay -->
  {#if showStartupChooser}
    <div class="startup-overlay">
      <div class="startup-card">
        <h2 class="startup-title">nTropy</h2>
        <p class="startup-subtitle">Open a project folder to start chatting and executing CLI tasks.</p>
        
        <button class="startup-browse-btn" onclick={async () => { await handleChoosePathDirect(); if (currentProjectPath) showStartupChooser = false; }} style="display: inline-flex; align-items: center; justify-content: center; gap: 0.5rem;">
          <svg class="folder-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" style="width: 16px; height: 16px;">
            <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"></path>
            <line x1="12" y1="11" x2="12" y2="17"></line>
            <line x1="9" y1="14" x2="15" y2="14"></line>
          </svg>
          Open Project Folder
        </button>

        {#if recentProjects.length > 0}
          <div class="startup-recent-section">
            <h3 class="startup-recent-title">Recent Projects</h3>
            <div class="startup-recent-list">
              {#each recentProjects as path}
                <button class="startup-recent-item" onclick={() => { handleOpenProject(path); showStartupChooser = false; }}>
                  <span class="recent-icon" style="display: inline-flex; align-items: center;">
                    <svg class="folder-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" style="width: 15px; height: 15px;">
                      <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"></path>
                    </svg>
                  </span>
                  <div class="recent-text">
                    <span class="recent-name">{path.split('\\').pop() || path.split('/').pop() || path}</span>
                    <span class="recent-path">{path}</span>
                  </div>
                </button>
              {/each}
            </div>
          </div>
        {/if}
      </div>
    </div>
  {/if}

  {#if showContextMenu}
    <div class="custom-context-menu" style="left: {contextMenuX}px; top: {contextMenuY}px;">
      <button class="context-menu-item danger" onclick={() => { if (contextMenuConvId) deleteConversation(contextMenuConvId); }}>
        <span class="menu-item-icon">🗑</span> Delete Conversation
      </button>
    </div>
  {/if}
</main>

<style>
  :root {
    --bg-dark: #121212;
    --bg-chat: #161616;
    --panel-bg: #121212;
    --border-glass: #222222;
    --border-glass-bright: #333333;
    --text-primary: #e2e2e3;
    --text-muted: #85858b;
    --cyan-glow: #ffffff;
    --purple-glow: #222222;
    --font-mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
  }

  :global(body) {
    background-color: var(--bg-dark);
    color: var(--text-primary);
    font-family: 'Inter', system-ui, -apple-system, sans-serif;
    margin: 0;
    padding: 0;
    overflow: hidden;
  }

  /* Premium Dark Scrollbars */
  :global(::-webkit-scrollbar) {
    width: 6px;
    height: 6px;
  }
  :global(::-webkit-scrollbar-track) {
    background: rgba(0, 0, 0, 0.1);
  }
  :global(::-webkit-scrollbar-thumb) {
    background: rgba(255, 255, 255, 0.05);
    border-radius: 999px;
  }
  :global(::-webkit-scrollbar-thumb:hover) {
    background: #333333;
  }

  .app-frame {
    display: flex;
    flex-direction: column;
    height: 100vh;
    background: var(--bg-dark);
  }

  /* Sleek Title & Menu Bar */
  .menu-bar {
    display: flex;
    align-items: center;
    background: #121212;
    border-bottom: 1px solid var(--border-glass);
    padding: 0.5rem 1.25rem;
    gap: 1.25rem;
    font-size: 0.85rem;
    user-select: none;
    z-index: 100;
  }

  .menu-item {
    color: var(--text-muted);
    cursor: pointer;
    font-weight: 500;
    transition: color 0.15s ease;
  }

  .menu-item:hover {
    color: var(--text-primary);
  }

  .menu-item.active-app {
    color: var(--text-primary);
    font-weight: 700;
  }

  .oauth-status-panel {
    display: flex;
    gap: 0.75rem;
    margin-left: auto;
    margin-right: 1rem;
    align-items: center;
  }

  .status-pill {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    background: transparent;
    border: none;
    font-size: 0.75rem;
    color: var(--text-muted);
  }

  .status-pill .dot {
    width: 5px;
    height: 5px;
    border-radius: 50%;
    display: inline-block;
  }

  .status-pill .dot.green {
    background: #10b981;
    box-shadow: 0 0 5px #10b981;
  }

  /* Toggle Tabs Button in Menu Bar */
  .toggle-tabs-btn {
    background: #1c1c1e;
    border: 1px solid var(--border-glass-bright);
    color: var(--text-primary);
    padding: 0.3rem 0.75rem;
    border-radius: 6px;
    font-size: 0.75rem;
    font-weight: 600;
    cursor: pointer;
    transition: all 0.15s ease;
  }

  .toggle-tabs-btn:hover {
    background: #27272a;
    border-color: #444448;
  }

  .toggle-tabs-btn.active {
    background: #27272a;
    border-color: #52525b;
  }

  /* Main Dashboard Grids */
  .dashboard-grid {
    display: flex;
    flex: 1;
    overflow: hidden;
  }

  .side-panel {
    width: 260px;
    background: #121212;
    border-right: 1px solid var(--border-glass);
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  .left-panel {
    transition: width 0.25s cubic-bezier(0.4, 0, 0.2, 1), opacity 0.25s ease, border-right-color 0.25s ease;
  }

  .left-panel.collapsed {
    width: 0;
    opacity: 0;
    border-right-color: transparent;
    pointer-events: none;
  }

  .right-panel {
    border-right: none;
    border-left: 1px solid var(--border-glass);
    width: 320px;
  }

  .panel-header {
    padding: 0.85rem 1rem;
    border-bottom: 1px solid var(--border-glass);
  }

  .panel-header h3 {
    margin: 0;
    font-size: 0.8rem;
    font-weight: 700;
    letter-spacing: 0.5px;
    color: var(--text-muted);
    text-transform: uppercase;
  }

  /* Sidebar Top Control Bar (matching the screenshot layout) */
  .sidebar-top-bar {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.75rem 1rem;
    border-bottom: 1px solid var(--border-glass);
  }

  .sidebar-tool-btn {
    background: transparent;
    border: none;
    color: var(--text-muted);
    font-size: 0.9rem;
    cursor: pointer;
    padding: 0.25rem 0.4rem;
    border-radius: 4px;
    transition: all 0.15s ease;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .sidebar-tool-btn:hover {
    background: #1c1c1e;
    color: var(--text-primary);
  }

  /* Sidebar Actions & Navigation */
  .sidebar-nav {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    padding: 0.5rem 0.5rem;
    border-bottom: 1px solid var(--border-glass);
  }

  .nav-btn {
    display: flex;
    align-items: center;
    gap: 0.65rem;
    width: 100%;
    background: transparent;
    border: none;
    color: var(--text-muted);
    padding: 0.5rem 0.75rem;
    text-align: left;
    cursor: pointer;
    font-size: 0.82rem;
    font-weight: 600;
    border-radius: 6px;
    transition: all 0.15s ease;
  }

  .nav-btn:hover {
    background: #1c1c1e;
    color: var(--text-primary);
  }

  .nav-btn .icon {
    font-size: 0.9rem;
    display: inline-block;
    width: 1rem;
    text-align: center;
  }


  /* Sidebar Sections */
  .sidebar-section {
    padding: 0.75rem 0.5rem;
    border-bottom: 1px solid var(--border-glass);
    display: flex;
    flex-direction: column;
  }

  .sidebar-section-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 0.25rem 0.5rem 0.5rem 0.5rem;
  }

  .section-title {
    font-size: 0.72rem;
    font-weight: 700;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .section-actions {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  .filter-icon {
    font-size: 0.8rem;
    color: var(--text-muted);
    cursor: pointer;
  }

  .add-project-icon-btn {
    background: transparent;
    border: none;
    color: var(--text-muted);
    font-size: 0.9rem;
    cursor: pointer;
    padding: 0;
    transition: color 0.15s ease;
  }

  .add-project-icon-btn:hover {
    color: var(--text-primary);
  }

  /* Projects List and Folder Blocks */
  .projects-list {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    max-height: 220px;
    overflow-y: auto;
  }

  .project-folder-block {
    display: flex;
    flex-direction: column;
    padding: 0.2rem 0.4rem;
    border-radius: 6px;
    transition: background 0.15s ease;
  }

  .project-folder-block:hover {
    background: #18181a;
  }

  .project-folder-block.active {
    background: #18181a;
  }

  .project-folder-btn {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    background: transparent;
    border: none;
    color: var(--text-primary);
    text-align: left;
    font-weight: 600;
    font-size: 0.82rem;
    cursor: pointer;
    padding: 0.25rem;
    width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .folder-svg {
    width: 14px;
    height: 14px;
    stroke: currentColor;
    display: inline-block;
    vertical-align: middle;
    transition: transform 0.15s ease;
    flex-shrink: 0;
  }

  .conv-btn {
    display: flex;
    align-items: center;
    justify-content: space-between;
    background: transparent;
    border: none;
    color: var(--text-muted);
    text-align: left;
    font-size: 0.78rem;
    cursor: pointer;
    padding: 0.35rem 0.5rem;
    width: 100%;
    border-radius: 6px;
    transition: all 0.15s ease;
  }

  .conv-btn:hover {
    color: var(--text-primary);
    background: rgba(255, 255, 255, 0.02);
  }

  .conv-btn.active {
    background: #27272a;
    color: var(--text-primary);
    font-weight: 500;
  }

  .conv-preview {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 140px;
  }

  .conv-time {
    color: var(--text-muted);
    font-size: 0.7rem;
    white-space: nowrap;
    margin-left: auto;
  }

  /* Conversations section and list */
  .conversations-list {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    max-height: 180px;
    overflow-y: auto;
  }

  .sidebar-conv-btn {
    display: flex;
    align-items: center;
    justify-content: space-between;
    background: transparent;
    border: none;
    color: var(--text-muted);
    text-align: left;
    font-size: 0.78rem;
    cursor: pointer;
    padding: 0.4rem 0.65rem;
    width: calc(100% - 0.5rem);
    margin: 0.1rem auto;
    border-radius: 6px;
    transition: all 0.15s ease;
  }

  .sidebar-conv-btn:hover {
    color: var(--text-primary);
    background: #18181a;
  }

  .sidebar-empty {
    font-size: 0.75rem;
    color: var(--text-muted);
    padding: 0.4rem 0.65rem;
    font-style: italic;
  }

  /* Collapsible Active Files Explorer */
  .collapsible-section {
    border-bottom: none;
    padding: 0;
  }

  .section-toggle-btn {
    display: flex;
    justify-content: space-between;
    align-items: center;
    width: 100%;
    background: transparent;
    border: none;
    color: var(--text-muted);
    padding: 0.75rem 1rem;
    cursor: pointer;
    text-align: left;
    border-bottom: 1px solid var(--border-glass);
    transition: all 0.15s ease;
  }

  .section-toggle-btn:hover {
    background: #18181a;
    color: var(--text-primary);
  }

  .compact-file-list {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    padding: 0.4rem;
    max-height: 150px;
    overflow-y: auto;
  }

  .compact-file-item {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    background: transparent;
    border: none;
    color: var(--text-muted);
    padding: 0.3rem 0.5rem;
    font-size: 0.78rem;
    text-align: left;
    cursor: pointer;
    border-radius: 4px;
    width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .compact-file-item:hover {
    background: #18181a;
    color: var(--text-primary);
  }

  .compact-file-item.selected {
    background: #27272a;
    color: var(--text-primary);
    font-weight: 600;
  }

  /* Main Workspace Center Chat Pane */
  .main-panel {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    background: var(--bg-chat);
    overflow: hidden;
  }

  .model-selection-bar {
    display: flex;
    align-items: center;
    padding: 0.5rem 1.25rem;
    background: #161616;
    border-bottom: 1px solid var(--border-glass);
    gap: 0.4rem;
  }

  .model-bar-title {
    font-size: 0.82rem;
    font-weight: 600;
    color: var(--text-muted);
    margin-right: 0.4rem;
    user-select: none;
  }

  .model-select-wrapper {
    position: relative;
    display: inline-flex;
    align-items: center;
  }

  .model-select-dropdown {
    appearance: none;
    -webkit-appearance: none;
    -moz-appearance: none;
    background: #1e1e20;
    border: 1px solid var(--border-glass-bright);
    color: var(--text-primary);
    padding: 0.35rem 1.85rem 0.35rem 0.65rem;
    border-radius: 6px;
    font-size: 0.78rem;
    font-weight: 600;
    outline: none;
    cursor: pointer;
    transition: all 0.15s ease;
  }

  .model-select-dropdown:hover {
    border-color: #52525b;
    background: #27272a;
  }

  .model-select-dropdown:focus {
    border-color: var(--text-primary);
  }

  .select-arrow {
    position: absolute;
    right: 0.65rem;
    font-size: 0.55rem;
    color: var(--text-muted);
    pointer-events: none;
    user-select: none;
  }

  .chat-container {
    flex: 1;
    display: flex;
    flex-direction: column;
    padding: 1.25rem;
    overflow: hidden;
  }

  .chat-scroller {
    flex: 1;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 0.85rem;
    padding-right: 0.4rem;
  }

  .message-card {
    background: #1c1c1e;
    border: 1px solid var(--border-glass);
    padding: 0.75rem 1.1rem;
    border-radius: 8px;
    max-width: 80%;
    align-self: flex-start;
  }

  .message-card.user {
    align-self: flex-end;
    background: #27272a;
    border: 1px solid #3f3f46;
  }

  .message-card.system {
    align-self: center;
    background: #121212;
    border: 1px solid var(--border-glass);
    color: var(--text-muted);
    font-size: 0.8rem;
    max-width: 95%;
  }

  .message-card.agent {
    align-self: flex-start;
    background: #18181a;
    border: 1px solid var(--border-glass);
  }

  .blinking-cursor {
    display: inline-block;
    width: 6px;
    height: 14px;
    background-color: var(--text-primary);
    margin-left: 2px;
    animation: blink-caret 0.75s step-end infinite;
    vertical-align: middle;
  }

  @keyframes blink-caret {
    from, to { background-color: transparent }
    50% { background-color: var(--text-primary); }
  }

  .msg-sender {
    font-size: 0.72rem;
    font-weight: 700;
    color: var(--text-muted);
    margin-bottom: 0.2rem;
  }

  .msg-text {
    font-size: 0.88rem;
    line-height: 1.45;
    white-space: pre-wrap;
  }

  /* Secure transaction approval card */
  .transaction-card {
    align-self: center;
    width: 90%;
    background: #271a1a;
    border: 1px solid rgba(239, 68, 68, 0.25);
    border-radius: 8px;
    padding: 1.1rem;
  }

  .card-title {
    margin: 0 0 0.4rem 0;
    color: #f87171;
    font-size: 0.9rem;
    font-weight: 700;
  }

  .card-body {
    font-size: 0.82rem;
    color: var(--text-primary);
    margin: 0 0 0.65rem 0;
  }

  .card-code {
    background: rgba(0, 0, 0, 0.25);
    padding: 0.5rem;
    border-radius: 4px;
    margin-bottom: 0.85rem;
  }

  .card-code code {
    font-family: var(--font-mono);
    font-size: 0.78rem;
    color: #fca5a5;
  }

  .card-actions {
    display: flex;
    gap: 0.65rem;
  }

  .approve-btn, .deny-btn {
    padding: 0.45rem 0.85rem;
    border-radius: 6px;
    cursor: pointer;
    font-weight: 600;
    font-size: 0.78rem;
    transition: all 0.15s ease;
  }

  .approve-btn {
    background: #ef4444;
    color: white;
    border: 1px solid #ef4444;
  }

  .approve-btn:hover {
    background: #dc2626;
  }

  .deny-btn {
    background: transparent;
    color: var(--text-muted);
    border: 1px solid var(--border-glass-bright);
  }

  .deny-btn:hover {
    background: rgba(255, 255, 255, 0.03);
    color: white;
  }

  /* Subprocess Terminal logs */
  .runtime-dock {
    flex: 0 0 auto;
    border-top: 1px solid var(--border-glass);
    background: #101012;
    padding: 0.6rem 1rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }

  .runtime-usage-strip {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    min-height: 24px;
    overflow-x: auto;
    white-space: nowrap;
  }

  .runtime-label {
    flex: 0 0 auto;
    color: var(--text-muted);
    font-size: 0.68rem;
    font-weight: 800;
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .runtime-usage-chip {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    background: #18181a;
    border: 1px solid var(--border-glass);
    border-radius: 999px;
    color: var(--text-muted);
    font-size: 0.68rem;
    padding: 0.2rem 0.5rem;
  }

  .runtime-usage-chip strong {
    color: var(--text-primary);
    font-size: 0.68rem;
  }

  .runtime-empty {
    color: var(--text-muted);
    font-size: 0.72rem;
  }

  .terminal-panel {
    height: 160px;
    background: #09090b;
    border: 1px solid var(--border-glass);
    border-radius: 6px;
    margin-top: 0.85rem;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  .terminal-header {
    background: rgba(255, 255, 255, 0.01);
    padding: 0.35rem 0.75rem;
    border-bottom: 1px solid var(--border-glass);
    display: flex;
    justify-content: space-between;
    font-size: 0.72rem;
    font-weight: 600;
    color: var(--text-muted);
  }

  .terminal-header-actions {
    display: flex;
    align-items: center;
    gap: 0.55rem;
  }

  .term-status {
    color: var(--text-muted);
  }

  .terminal-action-btn {
    background: #18181a;
    border: 1px solid var(--border-glass);
    border-radius: 4px;
    color: var(--text-muted);
    cursor: pointer;
    font-size: 0.68rem;
    font-weight: 700;
    padding: 0.12rem 0.45rem;
  }

  .terminal-action-btn:hover {
    border-color: var(--border-glass-bright);
    color: var(--text-primary);
  }

  .terminal-content {
    flex: 1;
    padding: 0.5rem;
    font-family: var(--font-mono);
    font-size: 0.78rem;
    overflow-y: auto;
    color: #a1a1aa;
  }

  .log-line {
    line-height: 1.4;
    white-space: pre-wrap;
  }

  .inline-terminal {
    height: 150px;
    margin-top: 0;
  }

  .tab-content-wrapper {
    flex: 1;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  /* Chat form */
  .input-form {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.75rem;
  }

  .input-form input {
    flex: 1;
    padding: 0.65rem 0.85rem;
    background: #18181a;
    border: 1px solid var(--border-glass-bright);
    border-radius: 8px;
    color: white;
    outline: none;
    font-size: 0.88rem;
    transition: all 0.15s ease;
  }

  .input-form input:focus {
    border-color: #52525b;
  }

  .input-form button {
    padding: 0 1.25rem;
    background: #27272a;
    border: 1px solid var(--border-glass-bright);
    color: var(--text-primary);
    border-radius: 8px;
    font-weight: 600;
    cursor: pointer;
    transition: all 0.15s ease;
  }

  .input-form button:hover {
    background: #3f3f46;
    border-color: #52525b;
  }

  /* Proof of life loaders & animations */
  .processing-loader-card {
    background: linear-gradient(135deg, rgba(20, 20, 25, 0.7) 0%, rgba(10, 10, 12, 0.9) 100%);
    border: 1px solid rgba(139, 92, 246, 0.2);
    border-radius: 8px;
    padding: 0.85rem 1rem;
    margin-top: 0.75rem;
    box-shadow: 0 4px 20px rgba(0, 0, 0, 0.4), inset 0 1px 0 rgba(255, 255, 255, 0.05);
    animation: breathingGlow 3s infinite ease-in-out;
  }

  .loader-header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin-bottom: 0.25rem;
  }

  .pulse-dot {
    width: 8px;
    height: 8px;
    background-color: #8b5cf6;
    border-radius: 50%;
    box-shadow: 0 0 8px #8b5cf6, 0 0 15px #8b5cf6;
    animation: dotPulse 1.2s infinite ease-in-out;
  }

  .loader-title {
    font-size: 0.7rem;
    font-family: var(--font-mono);
    color: #a78bfa;
    font-weight: 700;
    letter-spacing: 0.1em;
  }

  .loader-subtitle {
    margin: 0 0 0.65rem 0;
    font-size: 0.8rem;
    color: var(--text-muted);
  }

  .progress-bar-container {
    width: 100%;
    height: 3px;
    background: rgba(255, 255, 255, 0.03);
    border-radius: 999px;
    overflow: hidden;
    position: relative;
    border: 1px solid rgba(255, 255, 255, 0.01);
  }

  .scanning-laser {
    position: absolute;
    top: 0;
    bottom: 0;
    left: 0;
    width: 30%;
    background: linear-gradient(90deg, transparent 0%, #8b5cf6 50%, transparent 100%);
    box-shadow: 0 0 10px #8b5cf6;
    border-radius: 999px;
    animation: laserScan 2s infinite ease-in-out;
  }

  @keyframes breathingGlow {
    0%, 100% {
      border-color: rgba(139, 92, 246, 0.15);
      box-shadow: 0 4px 20px rgba(0, 0, 0, 0.4);
    }
    50% {
      border-color: rgba(139, 92, 246, 0.45);
      box-shadow: 0 4px 25px rgba(139, 92, 246, 0.15);
    }
  }

  @keyframes dotPulse {
    0%, 100% {
      transform: scale(0.8);
      opacity: 0.5;
    }
    50% {
      transform: scale(1.2);
      opacity: 1;
    }
  }

  @keyframes laserScan {
    0% {
      left: -30%;
    }
    100% {
      left: 100%;
    }
  }

  /* Enhancing active subagent card glowing border and pulse */
  .subagent-card.active {
    border-color: rgba(16, 185, 129, 0.35) !important;
    box-shadow: 0 0 8px rgba(16, 185, 129, 0.05);
    animation: activeAgentPulse 4s infinite ease-in-out;
  }

  @keyframes activeAgentPulse {
    0%, 100% {
      border-color: rgba(16, 185, 129, 0.25);
      box-shadow: 0 0 8px rgba(16, 185, 129, 0.03);
    }
    50% {
      border-color: rgba(16, 185, 129, 0.6);
      box-shadow: 0 0 12px rgba(16, 185, 129, 0.15);
    }
  }

  /* Right Control Sidebars */
  .tab-bar {
    display: flex;
    background: #121212;
    border-bottom: 1px solid var(--border-glass);
  }

  .tab-btn {
    flex: 1;
    background: transparent;
    border: none;
    padding: 0.65rem;
    color: var(--text-muted);
    font-size: 0.78rem;
    font-weight: 600;
    cursor: pointer;
    border-bottom: 2px solid transparent;
    transition: all 0.15s ease;
  }

  .tab-btn.active {
    color: var(--text-primary);
    border-bottom: 2px solid var(--text-primary);
    background: rgba(255, 255, 255, 0.01);
  }

  /* Subagent list rendering */
  .subagent-list {
    padding: 0.5rem;
    display: flex;
    flex-direction: column;
    gap: 0.65rem;
    overflow-y: auto;
    flex: 1;
  }

  .subagent-card {
    background: #18181a;
    border: 1px solid var(--border-glass);
    border-radius: 6px;
    padding: 0.65rem;
    transition: all 0.15s ease;
  }

  .usage-panel {
    background: #121214;
    border: 1px solid var(--border-glass);
    border-radius: 6px;
    padding: 0.65rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }

  .usage-title {
    font-size: 0.68rem;
    font-weight: 800;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .usage-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.65rem;
    border-top: 1px solid rgba(255, 255, 255, 0.04);
    padding-top: 0.45rem;
  }

  .usage-model {
    font-size: 0.72rem;
    font-weight: 700;
    color: var(--text-primary);
  }

  .usage-meta,
  .usage-empty {
    font-size: 0.68rem;
    color: var(--text-muted);
  }

  .usage-total {
    font-family: var(--font-mono);
    font-size: 0.76rem;
    color: #d4d4d8;
    white-space: nowrap;
  }

  .subagent-card.active {
    border-color: var(--border-glass-bright);
  }

  .card-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 0.2rem;
  }

  .sa-name {
    font-weight: 700;
    font-size: 0.82rem;
  }

  .sa-status {
    font-size: 0.62rem;
    padding: 0.1rem 0.3rem;
    border-radius: 4px;
    background: rgba(255, 255, 255, 0.03);
    color: var(--text-muted);
  }

  .sa-status.active {
    background: rgba(82, 82, 91, 0.15);
    color: #a1a1aa;
  }

  .sa-role {
    font-size: 0.72rem;
    color: var(--text-muted);
    margin-bottom: 0.45rem;
  }

  .sa-controls {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    font-size: 0.72rem;
  }

  .sa-controls select {
    background: #121212;
    color: var(--text-primary);
    border: 1px solid var(--border-glass);
    border-radius: 4px;
    padding: 0.25rem 0.5rem;
    font-size: 0.72rem;
    outline: none;
    cursor: pointer;
    transition: all 0.15s ease;
  }

  .sa-controls select:hover {
    border-color: var(--border-glass-bright);
  }

  .sa-run-btn {
    width: 100%;
    background: #27272a;
    border: 1px solid var(--border-glass-bright);
    color: var(--text-primary);
    border-radius: 5px;
    padding: 0.38rem 0.55rem;
    font-size: 0.72rem;
    font-weight: 700;
    cursor: pointer;
    transition: all 0.15s ease;
  }

  .sa-run-btn:hover {
    background: #3f3f46;
    border-color: #52525b;
  }


  /* Skills Panel Rendering */
  .skills-panel {
    padding: 0.5rem;
    display: flex;
    flex-direction: column;
    gap: 0.65rem;
    flex: 1;
    overflow-y: auto;
  }

  .skills-index {
    display: flex;
    flex-direction: column;
    gap: 0.45rem;
  }

  .skill-chip {
    display: flex;
    flex-direction: column;
    width: 100%;
    background: #18181a;
    border: 1px solid var(--border-glass);
    padding: 0.55rem;
    border-radius: 6px;
    text-align: left;
    cursor: pointer;
    transition: all 0.15s ease;
  }

  .skill-chip:hover {
    border-color: var(--border-glass-bright);
    background: #1e1e20;
  }

  .chip-title {
    font-size: 0.78rem;
    font-weight: 700;
    color: var(--text-primary);
  }

  .chip-desc {
    font-size: 0.68rem;
    color: var(--text-muted);
    margin-top: 0.1rem;
  }

  .skill-detail {
    background: #09090b;
    border: 1px solid var(--border-glass);
    border-radius: 6px;
    padding: 0.5rem;
    margin-top: 0.75rem;
  }

  .skill-detail h4 {
    margin: 0 0 0.4rem 0;
    font-size: 0.78rem;
    color: var(--text-primary);
  }

  .skill-detail pre {
    margin: 0;
    font-family: var(--font-mono);
    font-size: 0.72rem;
    white-space: pre-wrap;
    color: var(--text-primary);
  }

  /* Rules Checklist View */
  .rules-view-panel {
    padding: 0.5rem;
    overflow-y: auto;
    flex: 1;
  }

  .rules-checklist {
    display: flex;
    flex-direction: column;
    gap: 0.65rem;
  }

  .rule-label {
    display: flex;
    align-items: flex-start;
    gap: 0.45rem;
    font-size: 0.78rem;
    color: var(--text-primary);
    cursor: pointer;
    padding: 0.35rem;
    border-radius: 6px;
    background: #18181a;
    border: 1px solid var(--border-glass);
  }

  .rule-label:hover {
    background: #1e1e20;
  }

  .rule-label input {
    margin-top: 0.15rem;
  }

  .rule-text.blocking {
    color: #f87171;
  }

  /* Startup chooser Modal Overlay Styles */
  .startup-overlay {
    position: fixed;
    top: 0;
    left: 0;
    right: 0;
    bottom: 0;
    background: rgba(10, 10, 10, 0.75);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
    backdrop-filter: blur(4px);
  }

  .startup-card {
    background: #161616;
    border: 1px solid var(--border-glass-bright);
    border-radius: 12px;
    padding: 1.75rem;
    max-width: 440px;
    width: 90%;
    box-shadow: 0 10px 30px rgba(0, 0, 0, 0.4);
    display: flex;
    flex-direction: column;
    align-items: center;
    text-align: center;
  }

  .startup-title {
    margin: 0 0 0.4rem 0;
    font-size: 1.35rem;
    font-weight: 800;
    letter-spacing: 1px;
    color: var(--text-primary);
  }

  .startup-subtitle {
    font-size: 0.85rem;
    color: var(--text-muted);
    margin: 0 0 1.25rem 0;
    line-height: 1.45;
  }

  .startup-browse-btn {
    background: #27272a;
    border: 1px solid var(--border-glass-bright);
    color: var(--text-primary);
    font-weight: 600;
    font-size: 0.9rem;
    padding: 0.65rem 1.25rem;
    border-radius: 6px;
    cursor: pointer;
    transition: all 0.15s ease;
    width: 100%;
    margin-bottom: 1.25rem;
  }

  .startup-browse-btn:hover {
    background: #3f3f46;
    border-color: #52525b;
  }

  .startup-recent-section {
    width: 100%;
    border-top: 1px solid var(--border-glass);
    padding-top: 1rem;
    text-align: left;
  }

  .startup-recent-title {
    font-size: 0.75rem;
    font-weight: 700;
    color: var(--text-muted);
    text-transform: uppercase;
    margin: 0 0 0.65rem 0;
    letter-spacing: 0.5px;
  }

  .startup-recent-list {
    display: flex;
    flex-direction: column;
    gap: 0.45rem;
    max-height: 160px;
    overflow-y: auto;
  }

  .startup-recent-item {
    display: flex;
    align-items: center;
    gap: 0.65rem;
    background: #1c1c1e;
    border: 1px solid var(--border-glass);
    padding: 0.55rem;
    border-radius: 6px;
    cursor: pointer;
    text-align: left;
    width: 100%;
    transition: all 0.15s ease;
  }

  .startup-recent-item:hover {
    background: #27272a;
    border-color: var(--border-glass-bright);
  }

  .recent-text {
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  .recent-name {
    font-size: 0.78rem;
    font-weight: 700;
    color: var(--text-primary);
  }

  .recent-path {
    font-size: 0.65rem;
    color: var(--text-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* ========================================== */
  /* Scheduled Tasks Dashboard & Elements CSS  */
  /* ========================================== */
  .tasks-dashboard {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    padding: 1.5rem;
    overflow-y: auto;
    background: var(--bg-chat);
  }

  .tasks-dashboard .dashboard-header {
    margin-bottom: 1.5rem;
  }

  .tasks-dashboard .dashboard-header h2 {
    margin: 0 0 0.4rem 0;
    font-size: 1.25rem;
    font-weight: 800;
    color: var(--text-primary);
  }

  .tasks-dashboard .dashboard-header p {
    margin: 0;
    font-size: 0.85rem;
    color: var(--text-muted);
  }

  /* Task creation form card */
  .task-form-card {
    background: #18181a;
    border: 1px solid var(--border-glass);
    border-radius: 8px;
    padding: 1.25rem;
    margin-bottom: 2rem;
  }

  .task-form-card h3 {
    margin: 0 0 1rem 0;
    font-size: 0.9rem;
    font-weight: 700;
    color: var(--text-primary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .task-creation-form {
    display: flex;
    flex-direction: column;
    gap: 1rem;
  }

  .form-group {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }

  .form-group label {
    font-size: 0.72rem;
    font-weight: 700;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .form-group input,
  .form-group select {
    background: #121212;
    border: 1px solid var(--border-glass-bright);
    color: var(--text-primary);
    padding: 0.55rem 0.75rem;
    border-radius: 6px;
    font-size: 0.85rem;
    outline: none;
    transition: all 0.15s ease;
  }

  .form-group input:focus,
  .form-group select:focus {
    border-color: #52525b;
  }

  .form-row {
    display: flex;
    gap: 1rem;
  }

  .form-group.half {
    flex: 1;
  }

  .submit-task-btn {
    background: #27272a;
    border: 1px solid var(--border-glass-bright);
    color: var(--text-primary);
    padding: 0.6rem 1.25rem;
    border-radius: 6px;
    font-size: 0.85rem;
    font-weight: 600;
    cursor: pointer;
    transition: all 0.15s ease;
    align-self: flex-start;
  }

  .submit-task-btn:hover {
    background: #3f3f46;
    border-color: #52525b;
  }

  /* Tasks list and grid container */
  .tasks-list-container h3 {
    margin: 0 0 1rem 0;
    font-size: 0.9rem;
    font-weight: 700;
    color: var(--text-primary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .tasks-empty-state {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    padding: 3rem 1rem;
    background: #18181a;
    border: 1px dashed var(--border-glass-bright);
    border-radius: 8px;
    text-align: center;
  }

  .tasks-empty-state .empty-icon {
    font-size: 2rem;
    margin-bottom: 0.75rem;
    color: var(--text-muted);
  }

  .tasks-empty-state p {
    margin: 0;
    font-size: 0.85rem;
    color: var(--text-muted);
  }

  .tasks-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
    gap: 1.25rem;
  }

  .task-card {
    background: #18181a;
    border: 1px solid var(--border-glass);
    border-radius: 8px;
    padding: 1.1rem;
    display: flex;
    flex-direction: column;
    gap: 0.85rem;
    transition: all 0.15s ease;
  }

  .task-card:hover {
    border-color: var(--border-glass-bright);
  }

  .task-card.running {
    border-color: #52525b;
  }

  .task-card-header {
    display: flex;
    align-items: center;
    gap: 0.6rem;
  }

  .task-status-indicator {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    display: inline-block;
  }

  .task-status-indicator.active {
    background: #3b82f6; /* Muted blue */
  }

  .task-status-indicator.paused {
    background: #71717a; /* Muted grey */
  }

  .task-status-indicator.running {
    background: #10b981; /* Muted green */
    animation: status-pulse 1.5s infinite ease-in-out;
  }

  @keyframes status-pulse {
    0%, 100% {
      opacity: 1;
      transform: scale(1);
    }
    50% {
      opacity: 0.4;
      transform: scale(1.2);
    }
  }

  .task-name {
    margin: 0;
    font-size: 0.9rem;
    font-weight: 700;
    color: var(--text-primary);
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .task-cli-badge {
    background: #27272a;
    border: 1px solid var(--border-glass-bright);
    color: var(--text-muted);
    padding: 0.15rem 0.4rem;
    border-radius: 4px;
    font-size: 0.68rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .task-details {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    padding: 0.5rem 0;
    border-top: 1px solid rgba(255, 255, 255, 0.02);
    border-bottom: 1px solid rgba(255, 255, 255, 0.02);
  }

  .detail-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 0.78rem;
  }

  .detail-label {
    color: var(--text-muted);
  }

  .detail-value {
    color: var(--text-primary);
    font-weight: 500;
  }

  .detail-value.status-text {
    font-weight: 700;
  }

  .detail-value.status-text.success {
    color: #34d399;
  }

  .detail-value.status-text.failed {
    color: #f87171;
  }

  code.detail-value {
    font-family: var(--font-mono);
    background: #121212;
    padding: 0.1rem 0.3rem;
    border-radius: 4px;
    max-width: 180px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .task-actions-row {
    display: flex;
    gap: 0.5rem;
  }

  .action-btn {
    flex: 1;
    padding: 0.4rem 0.75rem;
    border-radius: 6px;
    font-size: 0.78rem;
    font-weight: 600;
    cursor: pointer;
    transition: all 0.15s ease;
    text-align: center;
  }

  .action-btn.run-btn {
    background: #27272a;
    border: 1px solid var(--border-glass-bright);
    color: var(--text-primary);
  }

  .action-btn.run-btn:hover:not(:disabled) {
    background: #3f3f46;
    border-color: #52525b;
  }

  .action-btn.run-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .action-btn.pause-btn {
    background: transparent;
    border: 1px solid var(--border-glass-bright);
    color: var(--text-primary);
  }

  .action-btn.pause-btn:hover {
    background: rgba(255, 255, 255, 0.03);
  }

  .action-btn.delete-btn {
    background: transparent;
    border: 1px solid rgba(239, 68, 68, 0.2);
    color: #f87171;
  }

  .action-btn.delete-btn:hover {
    background: rgba(239, 68, 68, 0.05);
    border-color: rgba(239, 68, 68, 0.4);
  }

  .task-output-section {
    margin-top: 0.25rem;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }

  .output-header {
    font-size: 0.68rem;
    font-weight: 700;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .task-output-log {
    margin: 0;
    background: #09090b;
    border: 1px solid var(--border-glass);
    padding: 0.55rem;
    border-radius: 6px;
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: #a1a1aa;
    max-height: 120px;
    overflow-y: auto;
    white-space: pre-wrap;
  }

  /* Custom Context Menu Styling */
  .custom-context-menu {
    position: fixed;
    background: #1c1c1e;
    border: 1px solid var(--border-glass-bright);
    border-radius: 8px;
    padding: 4px;
    box-shadow: 0 8px 20px rgba(0, 0, 0, 0.4);
    min-width: 160px;
    display: flex;
    flex-direction: column;
    z-index: 10000;
  }

  .context-menu-item {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    background: transparent;
    border: none;
    color: var(--text-primary);
    padding: 0.5rem 0.75rem;
    text-align: left;
    cursor: pointer;
    font-size: 0.8rem;
    font-weight: 500;
    border-radius: 6px;
    transition: all 0.15s ease;
  }

  .context-menu-item:hover {
    background: #27272a;
  }

  .context-menu-item.danger {
    color: #f87171;
  }

  .context-menu-item.danger:hover {
    background: rgba(239, 68, 68, 0.15);
    color: #ef4444;
  }
  
  .menu-item-icon {
    font-size: 0.9rem;
  }
</style>
