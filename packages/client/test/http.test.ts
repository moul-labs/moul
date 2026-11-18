import { describe, expect, it } from 'vitest';
import { HttpTransport, type RecordEvent } from '../src/index.js';

/** A fetch that answers a stream of server-sent events, split across chunks at awkward places. */
function sseFetch(chunks: string[], seen: { url?: string; headers?: Headers }): typeof fetch {
  return (async (input: RequestInfo | URL, init?: RequestInit) => {
    seen.url = String(input);
    seen.headers = new Headers(init?.headers);
    const enc = new TextEncoder();
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        for (const c of chunks) controller.enqueue(enc.encode(c));
        // Stay open, as an event stream does, until aborted.
        init?.signal?.addEventListener('abort', () => controller.error(new DOMException('aborted', 'AbortError')));
      },
    });
    return new Response(body, { status: 200, headers: { 'Content-Type': 'text/event-stream' } });
  }) as typeof fetch;
}

describe('HttpTransport', () => {
  it('reads record events from a server-sent event stream', async () => {
    const seen: { url?: string; headers?: Headers } = {};
    const events: RecordEvent[] = [];
    const t = new HttpTransport({
      url: 'https://app.example/',
      fetch: sseFetch(
        [
          'event: connected\ndata: {"clientId":"c1"}\n\n',
          ': keep-alive\n\n',
          'event: create\ndata: {"action":"create","moul":"tasks","record":{"id":"t1"}}\n',
          '\nevent: update\ndata: {"action":"update","moul":"tasks",',
          '"record":{"id":"t1","title":"x"},"old_record":{"id":"t1"}}\n\n',
        ],
        seen,
      ),
    });
    const stop = t.stream({
      collection: 'tasks',
      headers: { Authorization: 'Bearer tok' },
      onEvent: (e) => events.push(e),
      onError: (e) => {
        throw e;
      },
    });
    await new Promise((r) => setTimeout(r, 20));
    stop();
    expect(seen.url).toBe('https://app.example/api/moul/tasks/subscribe');
    expect(seen.headers?.get('authorization')).toBe('Bearer tok');
    expect(events.map((e) => [e.action, (e.record as any).id])).toEqual([
      ['create', 't1'],
      ['update', 't1'],
    ]);
    expect(events[1].old_record).toEqual({ id: 't1' });
  });

  it('builds URLs and bodies as a server expects', async () => {
    const calls: { url: string; init: RequestInit }[] = [];
    const t = new HttpTransport({
      url: 'https://app.example',
      adminKey: 'k',
      fetch: (async (input: RequestInfo | URL, init?: RequestInit) => {
        calls.push({ url: String(input), init: init ?? {} });
        return new Response('{"ok":true}', { status: 200, headers: { 'Content-Type': 'application/json' } });
      }) as typeof fetch,
    });
    const r = await t.api({ method: 'post', path: '/api/moul/tasks/records?expand=a', query: { perPage: 5, x: undefined }, json: { a: 1 }, owner: true, id: 'c1:1' });
    expect(r.json).toEqual({ ok: true });
    expect(calls[0].url).toBe('https://app.example/api/moul/tasks/records?expand=a&perPage=5');
    const h = new Headers(calls[0].init.headers);
    expect(h.get('content-type')).toBe('application/json');
    expect(h.get('x-admin-key')).toBe('k');
    expect(h.get('moul-request-id')).toBe('c1:1');
    expect(calls[0].init.method).toBe('POST');
    await expect(t.op('status')).rejects.toMatchObject({ code: 'unsupported' });
  });
});
