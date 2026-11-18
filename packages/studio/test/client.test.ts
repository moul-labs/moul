import { beforeEach, describe, expect, it, vi } from 'vitest';

// Studio's requests go to the owner client; this one records them instead.
const sent: { method?: string; path: string; query?: unknown }[] = [];
vi.mock('../src/studio', () => ({
  moul: () => ({
    send: async (req: { method?: string; path: string; query?: unknown }) => {
      sent.push(req);
      return { status: 200, json: {} };
    },
  }),
}));

const { api, ApiError, splitEndpoint } = await import('../src/api/client');

describe('engine paths', () => {
  beforeEach(() => {
    sent.length = 0;
  });

  it('split off the query by hand, leaving the path as written', () => {
    expect(splitEndpoint('/api/moul/tasks/records?page=2&sort=-createdAt')).toEqual({
      path: '/api/moul/tasks/records',
      query: 'page=2&sort=-createdAt',
    });
    expect(splitEndpoint('/api/moul')).toEqual({ path: '/api/moul', query: '' });
    // Dots inside a segment are an ordinary ID.
    expect(splitEndpoint('/api/moul/tasks/records/a..b').path).toBe('/api/moul/tasks/records/a..b');
  });

  it('refuse a "." or ".." segment, plain or percent-encoded', () => {
    for (const path of [
      '/api/moul/tasks/records/..',
      '/api/moul/tasks/records/.',
      '/api/moul/tasks/records/%2e%2E',
      '/api/moul/tasks/records/%2E',
      '/api/moul/../schema',
      '/api/moul/tasks/records/..?expand=owner',
    ]) {
      expect(() => splitEndpoint(path), path).toThrow(ApiError);
    }
    try {
      splitEndpoint('/api/moul/tasks/records/..');
    } catch (e) {
      expect((e as InstanceType<typeof ApiError>).status).toBe(400);
    }
  });

  it('never let a record ID of ".." reach the collection', async () => {
    await expect(api.deleteRecord('tasks', '..')).rejects.toMatchObject({ name: 'ApiError', status: 400 });
    await expect(api.updateRecord('tasks', '.', { done: true })).rejects.toMatchObject({ status: 400 });
    await expect(api.getRecord('tasks', '..', { expand: 'owner' })).rejects.toMatchObject({ status: 400 });
    expect(sent).toEqual([]);
  });

  it('send IDs encoded, with their query apart', async () => {
    await api.deleteRecord('tasks', 'a/../b?c');
    await api.getRecord('tasks', 'x y', { expand: 'owner' });
    await api.listRecords('tasks', { page: 2, filter: 'done = true' });
    expect(sent.map(({ method, path, query }) => ({ method, path, query }))).toEqual([
      { method: 'DELETE', path: '/api/moul/tasks/records/a%2F..%2Fb%3Fc', query: '' },
      { method: 'GET', path: '/api/moul/tasks/records/x%20y', query: 'expand=owner' },
      { method: 'GET', path: '/api/moul/tasks/records', query: 'page=2&filter=done+%3D+true' },
    ]);
  });
});
