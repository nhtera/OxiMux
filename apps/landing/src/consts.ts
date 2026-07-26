/**
 * Single source of truth for facts that appear in more than one place on the
 * page. Everything here is verifiable against the repo README, LICENSE, or
 * docs/system-architecture.md. Nothing is invented marketing precision.
 */

export const SITE = {
  name: "OxiMux",
  url: "https://oximux.erai.dev",
  repo: "https://github.com/nhtera/OxiMux",
  tagline: "Many agents. One native cockpit.",
  heroLines: ["Many agents.", "One native cockpit."],
  description:
    "Open-source, Rust-native development cockpit for macOS. Spawn isolated git worktrees, run coding agents in parallel, and review every change through a full Git UX.",
  license: "Apache-2.0",
  platform: "macOS 13.0+",
  /**
   * Resolves to the newest published GitHub release. This 404s until a release
   * is actually tagged, so tag one before the site goes public.
   */
  download: "https://github.com/nhtera/OxiMux/releases/latest",
} as const;

/**
 * Agents OxiMux can drive today. Sourced from the README capability list and
 * crates/agents (AgentRuntime trait plus provider adapters).
 *
 * One list, two surfaces. The hero strip renders the entries that carry an
 * `icon`; the features section renders all of them with their mechanism. Kept
 * as a single array so the two can never drift out of sync.
 *
 * `icon` is an astro-icon local name resolving to src/icons/, which holds the
 * desktop app's own monochrome glyphs, so the web and the cockpit show the same
 * mark for the same agent.
 */
export const AGENTS = [
  { name: "Claude Code", note: "stream-json adapter", icon: "claude-code" },
  { name: "Codex", note: "app-server JSON-RPC", icon: "codex" },
  { name: "Pi", note: "RPC transport", icon: "pi" },
  { name: "OpenCode", note: "session import", icon: "opencode" },
  { name: "Copilot", note: "session import", icon: "copilot" },
  { name: "Any ACP agent", note: "Agent Client Protocol" },
] as const;

/**
 * Hero strip subset. Deliberately no "+N more" count: unlike the tools that
 * advertise one, the honest statement here is the open-ended ACP line, not an
 * invented number.
 */
export const AGENT_MARKS = AGENTS.filter(
  (agent): agent is (typeof AGENTS)[number] & { icon: string } => "icon" in agent
);

/**
 * Substrate claims. Each is a structural fact about how the app is built, not a
 * benchmark. No invented numbers.
 */
export const SUBSTRATE = [
  {
    label: "Rust",
    detail: "Edition 2024 end to end. No Electron, no bundled browser.",
  },
  {
    label: "GPUI",
    detail: "The GPU-accelerated UI framework behind Zed, rendered on Metal.",
  },
  {
    label: "Out-of-process PTY",
    detail: "Terminals run in a relay daemon and survive an app relaunch.",
  },
  {
    label: "Apache-2.0",
    detail: "Open source. Read the source, fork it, ship your own build.",
  },
] as const;

export const FAQ = [
  {
    q: "Is OxiMux free?",
    a: "Yes. It is open source under Apache-2.0. You bring your own agent CLIs and your own credentials.",
  },
  {
    q: "Does my code leave my machine?",
    a: "No. Agents run as local subprocesses and talk to their own providers exactly as they would in your terminal. OxiMux does not proxy inference, store credentials, or upload your repository anywhere.",
  },
  {
    q: "Which agents are supported?",
    a: "Claude Code, Codex, and Pi have dedicated adapters, and any agent that speaks the Agent Client Protocol works through the ACP runtime. Existing OpenCode and Copilot sessions can be imported from their own stores. Anything else that runs in a terminal runs in a terminal tab.",
  },
  {
    q: "Do I need new API keys or a new subscription?",
    a: "No. OxiMux launches each provider's own CLI as a subprocess, so it uses whatever plan and login that CLI already has. Nothing is resold.",
  },
  {
    q: "What platforms does it run on?",
    a: "macOS 13.0 or later in v1, with a companion mobile client for pairing a phone to a desktop host. Other desktop platforms are not supported yet.",
  },
  {
    q: "How stable is it?",
    a: "It is a working cockpit in active development and is used daily for real repository work, but it is pre-1.0. Expect rough edges and breaking changes.",
  },
] as const;
