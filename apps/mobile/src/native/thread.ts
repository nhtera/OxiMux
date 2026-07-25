/**
 * The transcript read model, mirroring the `agent-core` fold the Rust core runs.
 *
 * These types describe `ThreadSnapshot.threadJson` — raw `serde_json` output, so
 * the field names are **snake_case**, unlike the camelCase uniffi gives the
 * records it generates. That difference is deliberate on both sides: the record
 * fields are a curated FFI surface, while this is the desktop's own persisted
 * shape passed through verbatim so the phone renders exactly what the desktop
 * folded, with no second fold to keep in step.
 *
 * Rust enums here are **externally tagged** (serde's default): a data-carrying
 * variant is `{ VariantName: payload }` and a unit variant is the bare string
 * `"VariantName"`. `PermissionKind` is the one exception — it carries
 * `#[serde(rename_all = "snake_case")]`, so its values are lowercase.
 *
 * No field uses `skip_serializing_if`, so every optional key is always present
 * and reads `null` when empty — hence `T | null` throughout rather than `?:`.
 */

export type ChatImage = { media_type: string; data: string };

export type AssistantMessage = { text: string; thinking: string };

export type CheckpointState = { sha: string; show: boolean };

/** Routes a request to its card. Lowercase — the one snake_case enum. */
export type PermissionKind = 'tool' | 'plan' | 'mode' | 'mcp' | 'other';

export type PermissionSuggestion = { kind: string; label: string; raw: unknown };

/**
 * A pending permission prompt. Note this is the *entry* model's request, which
 * is a different Rust type from the event that created it: it carries no tool
 * input of its own, because the input already sits on the enclosing tool call.
 */
export type PermissionRequest = {
  request_id: string;
  kind: PermissionKind;
  description: string;
  suggestions: PermissionSuggestion[];
};

export type QuestionOption = { label: string; description: string };

export type AskQuestion = {
  id: string;
  header: string;
  question: string;
  options: QuestionOption[];
  /** PascalCase — only `PermissionKind` is renamed. */
  kind: 'SingleSelect' | 'MultiSelect';
  other_allowed: boolean;
  is_secret: boolean;
};

export type QuestionRequest = { request_id: string; questions: AskQuestion[] };

/**
 * One question's draft answer. `selected` holds option **labels** (not ids or
 * indices) and `custom` supersedes it when non-empty, matching how the tool
 * itself resolves the pair.
 */
export type QuestionAnswer = { selected: string[]; custom: string | null };

/** The answer set for a request, keyed by question **id** — not question text.
 * The host remaps ids to text when it builds the backend payload, so getting
 * this key wrong drops the answer silently rather than erroring. */
export type QuestionAnswers = {
  by_question: Record<string, QuestionAnswer>;
  response: string | null;
};

/** The resolved value for one question, or undefined when unanswered. Mirrors
 * the Rust `QuestionAnswer::resolved`, which decides what actually gets sent. */
export function resolvedAnswer(answer: QuestionAnswer | undefined): string | undefined {
  const custom = answer?.custom?.trim();
  if (custom) return custom;
  const labels = (answer?.selected ?? []).map((s) => s.trim()).filter(Boolean);
  return labels.length > 0 ? labels.join(', ') : undefined;
}

/** True when every question has a resolved answer — the submit gate. */
export function allAnswered(questions: AskQuestion[], answers: QuestionAnswers): boolean {
  return questions.every((q) => resolvedAnswer(answers.by_question[q.id]) !== undefined);
}

export type ToolCallStatus =
  | 'Pending'
  | 'InProgress'
  | 'Completed'
  | 'Rejected'
  | 'Canceled'
  | { WaitingForConfirmation: PermissionRequest }
  | { AwaitingAnswer: QuestionRequest }
  | { Failed: string };

export type ToolCall = {
  id: string;
  name: string;
  input: unknown;
  status: ToolCallStatus;
  result: string | null;
  structured: unknown | null;
  images: ChatImage[];
  /** Set when an ACP tool call embeds a live terminal we cannot mount yet. */
  terminal_id: string | null;
  kind: string | null;
  redact_result: boolean;
  subagent_log: string[];
};

export type TurnFileChange = { path: string; added: number; removed: number };

export type ThreadEntry =
  | { User: { text: string; images: ChatImage[]; checkpoint: CheckpointState | null } }
  | { Assistant: AssistantMessage }
  | { ToolCall: ToolCall }
  | { ContextCompaction: { summary: string } }
  | { TurnDiff: { files: TurnFileChange[]; diff: string | null } };

export type PlanEntryLite = {
  content: string;
  /** `"pending" | "in_progress" | "completed"` by convention — a free string on
   * the wire, so it is rendered rather than matched exhaustively. */
  status: string;
  priority: string;
};

export type TurnUsage = {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  context_window: number | null;
  cost_usd: number | null;
};

export type Thread = {
  entries: ThreadEntry[];
  model: string | null;
  permission_mode: string | null;
  title: string | null;
  turn_active: boolean;
  compacting: boolean;
  last_error: string | null;
  last_summary: string | null;
  plan: PlanEntryLite[] | null;
  usage: TurnUsage | null;
  live_usage: TurnUsage | null;
  context_window: number | null;
  session_cost_usd: number;
  slash_commands: string[];
};

/** An empty thread, for the render before the first snapshot lands. */
export const EMPTY_THREAD: Thread = {
  entries: [],
  model: null,
  permission_mode: null,
  title: null,
  turn_active: false,
  compacting: false,
  last_error: null,
  last_summary: null,
  plan: null,
  usage: null,
  live_usage: null,
  context_window: null,
  session_cost_usd: 0,
  slash_commands: [],
};

export function parseThread(json: string): Thread {
  return JSON.parse(json) as Thread;
}

// ── Narrowing helpers ────────────────────────────────────────────────────────
// An externally-tagged union does not narrow on a discriminant property, so each
// variant needs an `in` check. Centralized here to keep the components readable.

export function isUser(e: ThreadEntry): e is Extract<ThreadEntry, { User: unknown }> {
  return 'User' in e;
}

export function isAssistant(e: ThreadEntry): e is Extract<ThreadEntry, { Assistant: unknown }> {
  return 'Assistant' in e;
}

export function isToolCall(e: ThreadEntry): e is Extract<ThreadEntry, { ToolCall: unknown }> {
  return 'ToolCall' in e;
}

export function isCompaction(
  e: ThreadEntry
): e is Extract<ThreadEntry, { ContextCompaction: unknown }> {
  return 'ContextCompaction' in e;
}

export function isTurnDiff(e: ThreadEntry): e is Extract<ThreadEntry, { TurnDiff: unknown }> {
  return 'TurnDiff' in e;
}

/** The permission prompt blocking this call, if it is waiting on one. */
export function pendingPermission(call: ToolCall): PermissionRequest | undefined {
  const status = call.status;
  return typeof status === 'object' && 'WaitingForConfirmation' in status
    ? status.WaitingForConfirmation
    : undefined;
}

/** The question blocking this call, if it is waiting on one. */
export function pendingQuestion(call: ToolCall): QuestionRequest | undefined {
  const status = call.status;
  return typeof status === 'object' && 'AwaitingAnswer' in status
    ? status.AwaitingAnswer
    : undefined;
}

/** The failure message, if this call failed. */
export function failure(call: ToolCall): string | undefined {
  const status = call.status;
  return typeof status === 'object' && 'Failed' in status ? status.Failed : undefined;
}

/**
 * A short label for a tool call's status chip. Data-carrying variants are
 * folded into a word, since the detail is already rendered by the card body.
 */
export function statusLabel(status: ToolCallStatus): string {
  if (typeof status === 'string') return status;
  if ('WaitingForConfirmation' in status) return 'Needs approval';
  if ('AwaitingAnswer' in status) return 'Needs an answer';
  return 'Failed';
}
