<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";

  // Svelte 5 Runes for highly reactive state management
  let activeTab = $state("chat"); // Right-side panel tab: "chat" (subagents), "skills", "rules"
  let currentView = $state("chat"); // Main central panel view: "chat", "tasks"
  
  let prompt = $state("");
  let streamLogs = $state<string[]>([]);
  let activeStreamingMessageId = $state<string | null>(null);
  let streamTimeout: any = null;
  let rawStreamingText = "";

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
      
      return true;
    });

    return filteredLines.join("\n").trim();
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
    activeConversation ? activeConversation.activeModel : "claude"
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

  function createNewConversation(projectPath: string, initialTitle = "New Conversation"): string {
    const newId = `conv-${Date.now()}-${Math.random().toString(36).substr(2, 9)}`;
    const newConv: Conversation = {
      id: newId,
      projectPath,
      title: initialTitle,
      lastUpdated: Date.now(),
      messages: [],
      activeModel: "claude"
    };
    conversations = [newConv, ...conversations];
    activeConversationId = newId;
    saveConversations();
    return newId;
  }

  function handleNewConversationClick() {
    if (!currentProjectPath) return;
    createNewConversation(currentProjectPath);
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

  function deleteConversation(id: string) {
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
    saveConversations();
    showContextMenu = false;
  }

  function saveConversations() {
    try {
      localStorage.setItem("nTropy_conversations", JSON.stringify(conversations));
      localStorage.setItem("nTropy_active_conv_id", activeConversationId || "");
    } catch (e) {
      console.error("Failed to save conversations:", e);
    }
  }

  function loadConversations() {
    try {
      const stored = localStorage.getItem("nTropy_conversations");
      if (stored) {
        conversations = JSON.parse(stored);
      }
      const activeId = localStorage.getItem("nTropy_active_conv_id");
      if (activeId && conversations.some(c => c.id === activeId)) {
        activeConversationId = activeId;
      }
    } catch (e) {
      console.error("Failed to load conversations:", e);
    }
  }

  function addMessageToActiveConversation(sender: string, text: string, type: 'user' | 'agent' | 'system', id?: string) {
    if (!activeConversationId) return;
    conversations = conversations.map(c => {
      if (c.id === activeConversationId) {
        // Automatically set conversation title from first user prompt
        let title = c.title;
        if (title === "New Conversation" && type === "user") {
          title = text.length > 25 ? text.substring(0, 25) + "..." : text;
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
    saveConversations();
  }

  function updateStreamingMessageText(msgId: string, text: string) {
    if (!activeConversationId) return;
    conversations = conversations.map(c => {
      if (c.id === activeConversationId) {
        return {
          ...c,
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
    saveConversations();
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
      
      // Load or create a conversation for this project
      const projConvs = conversations.filter(c => c.projectPath === path);
      if (projConvs.length > 0) {
        // Sort by lastUpdated descending and pick the most recent one
        const sorted = [...projConvs].sort((a, b) => b.lastUpdated - a.lastUpdated);
        activeConversationId = sorted[0].id;
      } else {
        createNewConversation(path);
      }
      saveConversations();
      
      if (files.length > 0) {
        selectedFile = files[0];
        await loadSymbols(selectedFile);
      } else {
        fileSymbols = [];
      }
      
      await refreshSkills();
    } catch (e) {
      console.error("Failed to open project:", e);
      addMessageToActiveConversation("Workspace Engine", `❌ Error opening project: ${e}`, 'system');
    }
  }
  
  // Model mapping configuration
  const providerModelsMap: Record<string, string[]> = {
    claude: ["claude-3-5-sonnet", "claude-3-5-haiku", "claude-3-opus", "claude-3-7-sonnet"],
    gemini: ["gemini-2.0-flash", "gemini-2.0-pro", "gemini-1.5-pro", "gemini-1.5-flash"],
    grok: ["grok-2", "grok-beta", "grok-1.5"],
    codex: ["gpt-4o", "gpt-4o-mini", "o1-mini", "o1-preview", "o3-mini"]
  };

  let defaultProviderModels = $state<Record<string, string>>({
    claude: "claude-3-5-sonnet",
    gemini: "gemini-2.0-flash",
    grok: "grok-2",
    codex: "gpt-4o"
  });
  
  // Subagent Control Panel State
  let subagents = $state<Array<{ id: string, name: string, role: string, model: string, specificModel: string, active: boolean, cost: string }>>([
    { id: "sa-1", name: "Research Agent", role: "Codebase search & symbols", model: "grok", specificModel: "grok-2", active: true, cost: "$0.02" },
    { id: "sa-2b", name: "Backend Coder Agent", role: "Rust / API / backend services", model: "codex", specificModel: "gpt-4o", active: false, cost: "$0.03" },
    { id: "sa-2f", name: "Frontend Coder Agent", role: "Svelte / TS / styling design", model: "claude", specificModel: "claude-3-5-sonnet", active: false, cost: "$0.03" },
    { id: "sa-3", name: "Verification Agent", role: "Cargo check / test execution", model: "gemini", specificModel: "gemini-2.0-flash", active: true, cost: "$0.01" }
  ]);

  // nTropy Learning Loop Skills
  let skills = $state<Array<{ name: string, description: string, trigger_phrases: string[] }>>([]);
  let activeSkillText = $state("");
  let selectedSkillName = $state("");

  // Approval Request state (Mock transaction cards)
  let pendingApproval = $state<{ id: string, type: string, action: string, path: string } | null>(null);

  // Scheduled Tasks Functions
  function saveTasks() {
    try {
      localStorage.setItem("nTropy_tasks", JSON.stringify(tasks));
    } catch (e) {
      console.error("Failed to save tasks:", e);
    }
  }

  function loadTasks() {
    try {
      const stored = localStorage.getItem("nTropy_tasks");
      if (stored) {
        tasks = JSON.parse(stored);
      }
    } catch (e) {
      console.error("Failed to load tasks:", e);
    }
  }

  function createNewTask(name: string, command: string, cli: string, schedule: string) {
    if (!currentProjectPath) return;
    const newTask: ScheduledTask = {
      id: `task-${Date.now()}-${Math.random().toString(36).substr(2, 9)}`,
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
    saveTasks();
    streamLogs = [...streamLogs, `[TASK RUNNER] Registered task: "${name}"`];
  }

  function deleteTask(id: string) {
    tasks = tasks.filter(t => t.id !== id);
    saveTasks();
  }

  function toggleTaskStatus(id: string) {
    tasks = tasks.map(t => {
      if (t.id === id) {
        const nextStatus = t.status === 'active' ? 'paused' : 'active';
        return { ...t, status: nextStatus };
      }
      return t;
    });
    saveTasks();
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
    
    // Mark as running and clear output
    tasks = tasks.map(t => t.id === task.id ? { 
      ...t, 
      status: 'running', 
      lastOutput: `[${new Date().toLocaleTimeString()}] Task triggered...\n` 
    } : t);
    saveTasks();

    try {
      streamLogs = [...streamLogs, `[TASK RUNNER] Triggered task "${task.name}" using CLI "${task.cli.toUpperCase()}"`];
      
      await invoke("run_cli_prompt", {
        sessionId: `task-session-${task.id}`,
        model: task.cli,
        prompt: task.command,
        agentMappings: getAgentMappings(),
        providerModels: getProviderModels()
      });
    } catch (e) {
      tasks = tasks.map(t => t.id === task.id ? { 
        ...t, 
        status: t.schedule === 'once' ? 'paused' : 'active',
        lastRun: Date.now(),
        lastResult: 'failed',
        lastOutput: t.lastOutput + `\nExecution Error: ${e}\n` 
      } : t);
      saveTasks();
    }
  }

  onMount(() => {
    // 1. Load local historical state
    loadConversations();
    loadTasks();

    let unlistenCliOutput: any = null;
    let unlistenCliFinished: any = null;
    let unlistenSpawnSubagent: any = null;
    let schedulerInterval: any = null;

    async function init() {
      // 2. Listen for background subprocess streams from Rust
      unlistenCliOutput = await listen("cli-output", (event: any) => {
        const payload: any = event.payload;
        const formattedLog = `[${payload.stream.toUpperCase()}] ${payload.data}`;
        streamLogs = [...streamLogs, formattedLog];
        
        // Route output to task if it belongs to a task run session
        if (payload.session_id.startsWith("task-session-")) {
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
        
        // Stream to conversational chat bubble if this is stdout and we have an active stream ID
        if (payload.stream === "stdout" && activeStreamingMessageId) {
          rawStreamingText += payload.data;
          const sanitized = sanitizeStdout(rawStreamingText);
          updateStreamingMessageText(activeStreamingMessageId, sanitized);
          
          // Reset stream timeout
          if (streamTimeout) clearTimeout(streamTimeout);
          streamTimeout = setTimeout(() => {
            activeStreamingMessageId = null;
          }, 1200);
        }

        // Auto scroll terminal logs & chat scroller
        setTimeout(() => {
          const term = document.getElementById("terminal-screen");
          if (term) term.scrollTop = term.scrollHeight;

          const chatScroller = document.querySelector(".chat-scroller");
          if (chatScroller) {
            chatScroller.scrollTop = chatScroller.scrollHeight;
          }
        }, 10);
      });

      // 3. Listen for CLI task completion
      unlistenCliFinished = await listen("cli-finished", (event: any) => {
        const session_id: any = event.payload;
        if (session_id.startsWith("task-session-")) {
          const taskId = session_id.replace("task-session-", "");
          tasks = tasks.map(t => {
            if (t.id === taskId) {
              const finalStatus = t.schedule === 'once' ? 'paused' : 'active';
              return {
                ...t,
                status: finalStatus,
                lastRun: Date.now(),
                lastResult: 'success',
                lastOutput: t.lastOutput + `\n[${new Date().toLocaleTimeString()}] Task finished successfully.\n`
              };
            }
            return t;
          });
          saveTasks();
          streamLogs = [...streamLogs, `[TASK RUNNER] Task execution finished: ${taskId}`];
        }
      });

      // 4. Listen for dynamic subagent spawning commands intercepted in active CLI streams
      unlistenSpawnSubagent = await listen("spawn-subagent", (event: any) => {
        const payload: any = event.payload;
        const saId = `sa-spawned-${Date.now()}`;
        const provider = (payload.model || "claude").toLowerCase();
        const specificModel = defaultProviderModels[provider] || (providerModelsMap[provider] ? providerModelsMap[provider][0] : "claude-3-5-sonnet");
        
        const newSa = {
          id: saId,
          name: payload.name,
          role: payload.role,
          model: provider,
          specificModel: specificModel,
          active: true,
          cost: "$0.01"
        };
        
        subagents = [...subagents, newSa];
        addMessageToActiveConversation("Security Kernel", `🚀 Dynamic Subagent Spawned: "${payload.name}" (${payload.role}) utilizing model: ${provider.toUpperCase()} (${specificModel}) CLI`, 'system');
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
          
          // Load or create conversation for this project
          const projConvs = conversations.filter(c => c.projectPath === currentProjectPath);
          if (projConvs.length > 0) {
            const sorted = [...projConvs].sort((a, b) => b.lastUpdated - a.lastUpdated);
            activeConversationId = sorted[0].id;
          } else {
            createNewConversation(currentProjectPath);
          }
          
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

  async function sendPrompt() {
    if (!prompt.trim() || !activeConversationId) return;
    
    const currentPrompt = prompt;
    prompt = "";

    // Add User Message
    addMessageToActiveConversation("User", currentPrompt, 'user');

    // Highlight any parsed dynamic rule triggers
    const triggerViolations: any = await invoke("check_rules_action", { 
      trigger: "before-edit", 
      content: currentPrompt 
    });
    
    if (triggerViolations.length > 0) {
      addMessageToActiveConversation("Rules Gate", `⚠️ [DEV-RULES WARNING]: Triggered rule constraint - ${triggerViolations[0].text}`, 'system');
    }

    // Allocate streaming chat bubble for the agent response
    const streamMsgId = `agent-stream-${Date.now()}`;
    activeStreamingMessageId = streamMsgId;
    rawStreamingText = "";

    addMessageToActiveConversation(activeModel.toUpperCase() + " Agent", "", 'agent', streamMsgId);

    // Clear any previous timeout
    if (streamTimeout) {
      clearTimeout(streamTimeout);
      streamTimeout = null;
    }

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
        sessionId: "active-workspace-session",
        model: activeModel,
        prompt: currentPrompt,
        agentMappings: getAgentMappings(),
        providerModels: getProviderModels()
      });
    } catch (e) {
      updateStreamingMessageText(streamMsgId, `CLI Fail: ${e}`);
      activeStreamingMessageId = null;
    }
  }

  function handleModelSwitch(model: string) {
    if (!activeConversationId) return;
    conversations = conversations.map(c => {
      if (c.id === activeConversationId) {
        return {
          ...c,
          activeModel: model
        };
      }
      return c;
    });
    saveConversations();
  }

  function toggleSubagentModel(id: string, model: string) {
    subagents = subagents.map(sa => {
      if (sa.id === id) {
        const defaultSpecific = defaultProviderModels[model] || (providerModelsMap[model] ? providerModelsMap[model][0] : "");
        return { ...sa, model, specificModel: defaultSpecific };
      }
      return sa;
    });
  }

  function toggleSubagentSpecificModel(id: string, specificModel: string) {
    subagents = subagents.map(sa => {
      if (sa.id === id) {
        return { ...sa, specificModel };
      }
      return sa;
    });
  }

  function getProviderModels(): Record<string, string> {
    const models: Record<string, string> = { ...defaultProviderModels };
    subagents.forEach(sa => {
      if (sa.active) {
        models[sa.model] = sa.specificModel;
      }
    });
    return models;
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
              onchange={(e: any) => defaultProviderModels[activeModel] = e.target.value}
            >
              {#each providerModelsMap[activeModel] || [] as specificOption}
                <option value={specificOption}>{specificOption}</option>
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
              placeholder="Type prompt here... (e.g. check index symbols, save skill, run build)" 
              bind:value={prompt}
            />
            <button type="submit">Execute Action</button>
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
                          <option value={specificOption}>{specificOption}</option>
                        {/each}
                      </select>
                    </div>
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

        <!-- CLI Output Panel moved to the side at the bottom of the right panel -->
        <div class="terminal-panel side-terminal">
          <div class="terminal-header">
            <span>Terminal Stdout Log</span>
            <span class="term-status">ACTIVE WRAPPER</span>
          </div>
          <div id="terminal-screen" class="terminal-content">
            {#each streamLogs as log}
              <div class="log-line">{log}</div>
            {/each}
          </div>
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

  .project-conversation-link {
    padding-left: 1.25rem;
    margin-top: 0.1rem;
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
    display: flex;
    flex-direction: column;
    background: var(--bg-chat);
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

  .term-status {
    color: var(--text-muted);
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

  .side-terminal {
    height: 200px;
    background: #09090b;
    border: none;
    border-top: 1px solid var(--border-glass);
    border-radius: 0;
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
