// Type-level checks, compiled by `npm run typecheck` (tsconfig.test.json); never run.
import { createClient, type BaseRecord } from '../src/index.js';

interface TaskRecord extends BaseRecord {
  title: string;
  done?: boolean | null;
  attachment?: { id: string; url: string } | null;
}
interface UsersRecord extends BaseRecord {
  username: string;
  email?: string;
}
interface MoulSchema {
  tasks: TaskRecord;
  users: UsersRecord;
}

export async function typed() {
  const moul = createClient<MoulSchema>({ db: 'types' });
  const t = await moul.collection('tasks').create({ title: 'ok', done: false, attachment: new Blob(['x']) });
  const title: string = t.title;
  // @ts-expect-error: a misspelled field is a compile error
  await moul.collection('tasks').create({ titel: 'typo' });
  // @ts-expect-error: a wrong type is a compile error
  await moul.collection('tasks').update(t.id, { done: 'yes' });
  // Auth collections take password fields.
  await moul.collection('users').create({ username: 'ana', email: 'a@x.io', password: 'p', passwordConfirm: 'p' });
  // An untyped collection takes anything.
  await moul.collection('anything').create({ whatever: 1 });
  return title;
}
