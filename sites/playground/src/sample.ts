import type { MoulClient } from '@moul-dev/client';

/**
 * The sample app: people sign in, keep projects and tasks, and see only their own. It shows
 * relations, rules that use `@request.auth`, and sign-in codes in the local inbox.
 */
export const sampleSchema = {
  format: 1,
  collections: [
    {
      name: 'users',
      type: 'auth',
      fields: [{ name: 'name', type: 'text' }],
    },
    {
      name: 'projects',
      fields: [
        { name: 'name', type: 'text', required: true },
        { name: 'owner', type: 'relation', relationConfig: { targetMoul: 'users', cardinality: '1:N' } },
      ],
      rules: {
        listRule: 'owner = @request.auth.id',
        viewRule: 'owner = @request.auth.id',
        createRule: "@request.auth.id != '' && @request.body.owner = @request.auth.id",
        updateRule: 'owner = @request.auth.id',
        deleteRule: 'owner = @request.auth.id',
      },
    },
    {
      name: 'tasks',
      fields: [
        { name: 'title', type: 'text', required: true },
        { name: 'done', type: 'bool' },
        { name: 'due', type: 'date' },
        { name: 'project', type: 'relation', relationConfig: { targetMoul: 'projects', cardinality: '1:N', onDelete: 'CASCADE' } },
        { name: 'owner', type: 'relation', relationConfig: { targetMoul: 'users', cardinality: '1:N' } },
      ],
      rules: {
        listRule: 'owner = @request.auth.id',
        viewRule: 'owner = @request.auth.id',
        createRule: "@request.auth.id != '' && @request.body.owner = @request.auth.id",
        updateRule: 'owner = @request.auth.id',
        deleteRule: 'owner = @request.auth.id',
      },
    },
  ],
};

/** Applies the sample schema and adds a person with a project and a few tasks. */
export async function loadSample(client: MoulClient): Promise<void> {
  const owner = client.asOwner();
  await owner.admin.applySchema(sampleSchema);
  const ana = await owner.collection('users').create({
    username: 'ana',
    email: 'ana@example.com',
    name: 'Ana',
    password: 'Password123!',
    passwordConfirm: 'Password123!',
  });
  const home = await owner.collection('projects').create({ name: 'Home', owner: ana.id });
  const day = 24 * 3600 * 1000;
  const iso = (d: number) => new Date(Date.now() + d * day).toISOString().slice(0, 10);
  for (const [title, done, due] of [
    ['Buy oat milk', false, iso(0)],
    ['Call the plumber', false, iso(1)],
    ['Return the library books', true, iso(-2)],
  ] as const) {
    await owner.collection('tasks').create({ title, done, due, project: home.id, owner: ana.id });
  }
}
