// Studio's requests, sent to the Moul engine through @moul-dev/client as the database's owner.
// In a tab the person at the keyboard owns the data, so there is no sign-in to the console.
import { MoulError } from '@moul-dev/client';
import { emitApiRequest } from '../devtools/events';
import { moul } from '../studio';

export class ApiError extends Error {
  status: number;
  data: any;

  constructor(status: number, message: string, data?: any) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.data = data;
  }
}

function toApiError(err: unknown): ApiError {
  if (err instanceof ApiError) return err;
  if (err instanceof MoulError) return new ApiError(err.status, err.message, err.data);
  return new ApiError(0, err instanceof Error ? err.message : String(err));
}

/** Paths are engine paths; Studio never talks to a server of its own. */
export function resolveApiPath(path: string): string {
  return path;
}

function isDotSegment(segment: string): boolean {
  if (segment === '.' || segment === '..') return true;
  try {
    const decoded = decodeURIComponent(segment);
    return decoded === '.' || decoded === '..';
  } catch {
    return false;
  }
}

/**
 * Splits an engine path from its query string, by hand. A URL would resolve dot segments: a record
 * ID of `..` would turn `DELETE /api/moul/tasks/records/..` into `DELETE /api/moul/tasks/`, which
 * deletes the collection. A path with a `.` or `..` segment, plain or percent-encoded, is refused.
 */
export function splitEndpoint(endpoint: string): { path: string; query: string } {
  const at = endpoint.indexOf('?');
  const path = at === -1 ? endpoint : endpoint.slice(0, at);
  const query = at === -1 ? '' : endpoint.slice(at + 1);
  if (path.split('/').some(isDotSegment)) {
    throw new ApiError(400, `Studio does not send a path with a "." or ".." segment: ${path}`);
  }
  return { path, query };
}

type Body = { json?: unknown; form?: { fields: [string, string][]; files: [string, { name: string; type: string; data: Uint8Array }][] } };

async function bodyOf(init: RequestInit): Promise<Body> {
  const b = init.body;
  if (b === undefined || b === null) return {};
  if (typeof b === 'string') return { json: b ? JSON.parse(b) : undefined };
  if (b instanceof FormData) {
    const fields: [string, string][] = [];
    const files: [string, { name: string; type: string; data: Uint8Array }][] = [];
    for (const [k, v] of b.entries()) {
      if (typeof v === 'string') fields.push([k, v]);
      else files.push([k, { name: v.name, type: v.type || 'application/octet-stream', data: new Uint8Array(await v.arrayBuffer()) }]);
    }
    return { form: { fields, files } };
  }
  throw new ApiError(0, 'unsupported request body');
}

/** Sends one request as the owner and returns its body: JSON, text, or an empty object. */
async function request<T>(endpoint: string, init: RequestInit = {}): Promise<T> {
  const method = (init.method || 'GET').toUpperCase();
  const startTime = performance.now();
  try {
    const { path, query } = splitEndpoint(endpoint);
    const res = await moul().send({ method, path, query, ...(await bodyOf(init)), owner: true });
    emitApiRequest({ method, url: endpoint, status: res.status, durationMs: Math.round(performance.now() - startTime) });
    if (res.status >= 400) {
      const data = res.json;
      throw new ApiError(res.status, data?.message || data?.error || `Request failed with status ${res.status}`, data);
    }
    if (res.json !== undefined) return res.json as T;
    if (res.bytes) {
      const type = res.type || '';
      if (type.startsWith('text/')) return new TextDecoder().decode(res.bytes) as unknown as T;
      return res.bytes as unknown as T;
    }
    return {} as T;
  } catch (err) {
    const e = toApiError(err);
    if (e.status === 0) {
      emitApiRequest({ method, url: endpoint, status: 0, durationMs: Math.round(performance.now() - startTime), error: e.message });
    }
    throw e;
  }
}

function download(blob: Blob, name: string): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
}

const enc = encodeURIComponent;

export const api = {
  // Collections
  listMouls: () => request<any[]>('/api/moul'),
  getMoul: (name: string) => request<any>(`/api/moul/${enc(name)}`),
  createMoul: (data: any) => request<any>('/api/moul', { method: 'POST', body: JSON.stringify(data) }),
  updateMoul: (name: string, data: any) => request<any>(`/api/moul/${enc(name)}`, { method: 'PATCH', body: JSON.stringify(data) }),
  deleteMoul: (name: string) => request<any>(`/api/moul/${enc(name)}`, { method: 'DELETE' }),

  // Email templates: the engine keeps mail in the local inbox.
  getEmailTemplates: (name: string) => request<any>(`/api/moul/${enc(name)}/email-templates`),
  updateEmailTemplates: (name: string, data: any) =>
    request<any>(`/api/moul/${enc(name)}/email-templates`, { method: 'PUT', body: JSON.stringify(data) }),

  // Records
  listRecords: (name: string, params?: { page?: number; perPage?: number; sort?: string; filter?: string; search?: string; expand?: string }) => {
    const q = new URLSearchParams();
    if (params?.page) q.set('page', String(params.page));
    if (params?.perPage) q.set('perPage', String(params.perPage));
    if (params?.sort) q.set('sort', params.sort);
    if (params?.filter) q.set('filter', params.filter);
    if (params?.search) q.set('search', params.search);
    if (params?.expand) q.set('expand', params.expand);
    const qs = q.toString();
    return request<{ items: any[]; totalItems?: number; page?: number; perPage?: number } | any[]>(
      `/api/moul/${enc(name)}/records${qs ? `?${qs}` : ''}`,
    );
  },
  getRecord: (name: string, id: string, params?: { expand?: string }) => {
    const qs = params?.expand ? `?expand=${enc(params.expand)}` : '';
    return request<any>(`/api/moul/${enc(name)}/records/${enc(id)}${qs}`);
  },
  createRecord: (name: string, data: any) => request<any>(`/api/moul/${enc(name)}/records`, { method: 'POST', body: JSON.stringify(data) }),
  updateRecord: (name: string, id: string, data: any) =>
    request<any>(`/api/moul/${enc(name)}/records/${enc(id)}`, { method: 'PATCH', body: JSON.stringify(data) }),
  deleteRecord: (name: string, id: string) => request<any>(`/api/moul/${enc(name)}/records/${enc(id)}`, { method: 'DELETE' }),

  exportRecords: async (name: string, options?: { format?: 'csv' | 'json'; includeSchema?: boolean; filter?: string; sort?: string }) => {
    const format = options?.format || 'json';
    const query = new URLSearchParams({ format });
    if (options?.includeSchema) query.set('includeSchema', 'true');
    if (options?.filter) query.set('filter', options.filter);
    if (options?.sort) query.set('sort', options.sort);
    const { path } = splitEndpoint(`/api/moul/${enc(name)}/export`);
    const res = await moul().send({ path, query: query.toString(), owner: true });
    if (res.status >= 400) throw toApiError(new MoulError(res.status, res.json?.message || `Export failed with status ${res.status}`, res.json));
    const blob = res.bytes
      ? new Blob([res.bytes as BlobPart], { type: res.type || 'application/octet-stream' })
      : new Blob([JSON.stringify(res.json, null, 2)], { type: 'application/json' });
    download(blob, `${name}-records-${new Date().toISOString().slice(0, 10)}.${format}`);
  },
  importRecords: async (
    name: string,
    file: File,
    options?: { mode?: 'upsert' | 'insert' | 'replace'; onError?: 'atomic' | 'continue'; format?: 'csv' | 'json' },
  ): Promise<{
    success: boolean;
    mode: string;
    total: number;
    inserted: number;
    updated: number;
    skipped: number;
    errors?: Array<{ row: number; id?: string; message: string }>;
  }> => {
    const formData = new FormData();
    formData.append('file', file);
    if (options?.mode) formData.append('mode', options.mode);
    if (options?.onError) formData.append('onError', options.onError);
    if (options?.format) formData.append('format', options.format);
    return request(`/api/moul/${enc(name)}/import`, { method: 'POST', body: formData });
  },
  uploadFile: (file: File) => {
    const formData = new FormData();
    formData.append('file', file);
    return request<any>('/api/upload', { method: 'POST', body: formData });
  },

  // Moul's own
  schema: () => request<any>('/api/schema'),
  applySchema: (schema: unknown, prune = false) =>
    request<any>(`/api/schema${prune ? '?prune=true' : ''}`, { method: 'POST', body: JSON.stringify(schema) }),
  types: () => request<string>('/api/types'),
  changes: (after = 0, limit = 100) => request<{ items: any[]; last: number }>(`/api/changes?after=${after}&limit=${limit}`),
  inbox: () => request<{ items: any[] }>('/api/inbox'),
  testRule: (body: { rule: string; record?: object; auth?: object | null; request?: object }) =>
    request<{ valid: boolean; matched: boolean; translated: string; error?: string }>('/api/rules/test', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
};
