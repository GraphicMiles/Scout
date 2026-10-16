export interface ScoutClientOptions { baseUrl: string; apiKey?: string; timeoutMs?: number; maxRetries?: number; fetch?: typeof fetch }
export interface Warning { code: string; message: string; [k: string]: unknown }
export interface FieldSpec { selector?: string; attr?: string; type?: 'string' | 'url' | 'number' | 'price' | 'date'; multiple?: boolean }
export interface Schema { item?: string; fields: Record<string, string | FieldSpec> }
export interface ExtractParams {
  sessionId?: string; url?: string; strategy?: string; schema?: Schema; limit?: number; timeoutMs?: number;
}
export interface ExtractResult<T = unknown> { data: T; warnings: Warning[]; meta: { strategy: string; url: string; status: number; durationMs: number } }
export interface NavigateResult { url: string; status: number; contentType: string; bytes: number; title: string | null; redirects: { from: string; to: string; status: number }[]; warnings: Warning[]; durationMs: number }
export type ActionType = 'click' | 'fill' | 'select' | 'check' | 'uncheck' | 'press' | 'submit' | 'scroll';
export interface Action { type: ActionType; selector?: string; index?: number; value?: string | string[]; key?: string; timeoutMs?: number }
export interface InteractResult {
  /** "none" when the action only changed local state; "navigate" when it caused a request. */
  effect: 'none' | 'navigate';
  /** Present when effect is "none". */
  detail?: Record<string, unknown>;
  /** Present when effect is "navigate": the page the interaction loaded. */
  navigation?: NavigateResult;
  warnings?: Warning[];
}

export interface EvalResult {
  /** The script's return value, as JSON. Null when the script returned nothing. */
  value: unknown;
  logs: string[];
  durationMs: number;
  /** True when the script hit the wall-clock limit and was interrupted. */
  timedOut: boolean;
}
export type TaskStep =
  | { op: 'navigate'; url: string; timeoutMs?: number }
  | ({ op: 'extract' } & Omit<ExtractParams, 'sessionId'>)
  | { op: 'interact'; action: Action };
export interface TaskSpec { sessionId?: string; steps: TaskStep[]; timeoutMs?: number; retries?: number }
export type TaskStatus = 'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled' | 'timed_out';
export interface Task { id: string; status: TaskStatus; createdAt: string; startedAt: string | null; finishedAt: string | null; result: { steps: unknown[]; data?: unknown } | null; error: { code: string; message: string; retryable: boolean; details?: unknown } | null }
export interface RequestParams {
  sessionId?: string;
  url: string;
  method?: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE' | 'HEAD' | 'OPTIONS';
  headers?: Record<string, string>;
  body?: string;
  contentType?: string;
  timeoutMs?: number;
}
export interface RequestResult {
  url: string;
  status: number;
  statusText: string;
  contentType: string;
  headers: Record<string, string>;
  body: string;
  bytes: number;
  redirects: unknown[];
}
export interface LoginParams {
  sessionId: string;
  url: string;
  username: string;
  password: string;
  usernameSelector?: string;
  passwordSelector?: string;
  submitSelector?: string;
  usernameIndex?: number;
  passwordIndex?: number;
  fields?: Record<string, string>;
}
export interface LoginResult {
  looksAuthenticated: boolean;
  reason: string;
  url: string | null;
  title: string | null;
  cookies: { before: number; after: number; added: string[] };
  passwordFieldStillPresent: boolean;
  landing: { url: string; title: string; passwordFields: number; inputs: number; text: string } | { error: string };
  steps: unknown[];
}
export interface ToolDef { name: string; description: string; inputSchema: Record<string, unknown> }
export interface Strategy { name: string; description: string; status: 'verified-fixtures' | 'fixture-verified-only' | 'not_implemented'; reason?: string }
export class ScoutApiError extends Error { status: number; code: string; retryable: boolean; details?: unknown; retryAfterSec?: number }
export class ScoutClient {
  constructor(opts: ScoutClientOptions);
  health(): Promise<{ status: string; version: string; uptimeSec: number }>;
  strategies(): Promise<Strategy[]>;
  createSession(): Promise<{ id: string; createdAt: string; expiresInMs: number }>;
  destroySession(id: string): Promise<void>;
  navigate(p: { sessionId: string; url: string; timeoutMs?: number }, o?: { signal?: AbortSignal }): Promise<NavigateResult>;
  extract<T = unknown>(p: ExtractParams, o?: { signal?: AbortSignal }): Promise<ExtractResult<T>>;
  interact(p: { sessionId: string; action: Action }, o?: { signal?: AbortSignal }): Promise<InteractResult>;

  /** Execute JavaScript against the session's loaded page (read-only DOM snapshot). */
  eval(p: { sessionId: string; script: string }, o?: { signal?: AbortSignal }): Promise<EvalResult>;

  /** A request with the session's cookies, returned unparsed, under the SSRF policy. */
  request(p: RequestParams, o?: { signal?: AbortSignal }): Promise<RequestResult>;
  /** Discover the agent-facing tool layer. */
  tools(): Promise<ToolDef[]>;
  callTool(name: string, args?: Record<string, unknown>, o?: { signal?: AbortSignal }): Promise<unknown>;
  /** Sign in with credentials you are authorised to use; resolves with the evidence. */
  login(p: LoginParams, o?: { signal?: AbortSignal }): Promise<LoginResult>;
  exportSession(sessionId: string): Promise<{ sessionId: string; referrer: string | null; cookies: unknown[] }>;
  importSession(sessionId: string, url: string, cookies: unknown[]): Promise<{ ok: true; imported: number; cookies: number }>;
  submitTask(spec: TaskSpec): Promise<Task>;
  getTask(id: string): Promise<Task>;
  cancelTask(id: string): Promise<Task>;
  withSession<T>(fn: (sessionId: string) => Promise<T>): Promise<T>;
  waitForTask(id: string, o?: { pollMs?: number; timeoutMs?: number; signal?: AbortSignal }): Promise<Task>;
  runTask(spec: TaskSpec, o?: { pollMs?: number; timeoutMs?: number }): Promise<Task>;
}
