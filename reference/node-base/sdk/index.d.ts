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
export interface InteractResult { ok: true; type: ActionType; navigated: boolean; navigation?: NavigateResult; page?: { url: string; status: number; title: string | null } }
export type TaskStep =
  | { op: 'navigate'; url: string; timeoutMs?: number }
  | ({ op: 'extract' } & Omit<ExtractParams, 'sessionId'>)
  | { op: 'interact'; action: Action };
export interface TaskSpec { sessionId?: string; steps: TaskStep[]; timeoutMs?: number; retries?: number }
export type TaskStatus = 'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled' | 'timed_out';
export interface Task { id: string; status: TaskStatus; createdAt: string; startedAt: string | null; finishedAt: string | null; result: { steps: unknown[]; data?: unknown } | null; error: { code: string; message: string; retryable: boolean; details?: unknown } | null }
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
  submitTask(spec: TaskSpec): Promise<Task>;
  getTask(id: string): Promise<Task>;
  cancelTask(id: string): Promise<Task>;
  withSession<T>(fn: (sessionId: string) => Promise<T>): Promise<T>;
  waitForTask(id: string, o?: { pollMs?: number; timeoutMs?: number; signal?: AbortSignal }): Promise<Task>;
  runTask(spec: TaskSpec, o?: { pollMs?: number; timeoutMs?: number }): Promise<Task>;
}
