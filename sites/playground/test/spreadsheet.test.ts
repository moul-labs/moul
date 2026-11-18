import { createHeadlessClient } from '@moul-dev/client/headless';
import { describe, expect, it } from 'vitest';
import { addSpreadsheet, collectionName, columnType, fieldName, parseCsv, planSheet } from '../src/spreadsheet';
import { loadSample } from '../src/sample';

describe('spreadsheets', () => {
  it('parse quotes, commas and line breaks in cells', () => {
    expect(parseCsv('a,b\r\n"x, y","say ""hi""\nthere"\n\n1,2')).toEqual([
      ['a', 'b'],
      ['x, y', 'say "hi"\nthere'],
      ['1', '2'],
    ]);
  });

  it('name fields and pick their types', () => {
    const taken = new Set<string>();
    expect(fieldName('Due Date', taken)).toBe('dueDate');
    expect(fieldName('due_date', taken)).toBe('dueDate2');
    expect(fieldName('2024 total', taken)).toBe('c2024Total');
    expect(fieldName('ID', taken)).toBe('idField');
    expect(fieldName('Café ☕', taken)).toBe('cafe');
    expect(fieldName('☕', taken)).toBe('column');
    expect(columnType(['1', '2.5', '', '-3'])).toBe('number');
    expect(columnType(['yes', 'No', 'TRUE'])).toBe('bool');
    expect(columnType(['1', 'two'])).toBe('text');
    expect(collectionName('My Books (2024).csv')).toBe('my_books_2024');
    expect(collectionName('2024.csv')).toBe('sheet_2024');
    expect(collectionName('SQLite_master.csv')).toBe('sheet_sqlite_master');
  });

  it('keep a column as text when numbers would lose something', () => {
    expect(columnType(['0', '0.5', '-0.25', '10', '1e3', '2.5E-4'])).toBe('number');
    // Leading zeros: codes, not amounts.
    expect(columnType(['01234', '98765'])).toBe('text');
    expect(columnType(['-007'])).toBe('text');
    expect(columnType(['00.5'])).toBe('text');
    // Whole numbers a double cannot hold exactly.
    expect(columnType([String(Number.MAX_SAFE_INTEGER), '1'])).toBe('number');
    expect(columnType([String(Number.MAX_SAFE_INTEGER + 1)])).toBe('text');
    expect(columnType(['12345678901234567890'])).toBe('text');
    // Numbers that overflow to Infinity.
    expect(columnType(['1e999'])).toBe('text');
    expect(columnType([`1${'0'.repeat(400)}.5`])).toBe('text');
    const plan = planSheet('Zip,Card,Big\n01234,4111111111111111111,1e400\n90210,1,2\n');
    expect(plan.fields.map((f) => f.type)).toEqual(['text', 'text', 'text']);
    expect(plan.records[0]).toEqual({ zip: '01234', card: '4111111111111111111', big: '1e400' });
  });

  it('become a collection with one record per row', async () => {
    const moul = await createHeadlessClient({ owner: true });
    const csv = 'Title,Page count,Read\nDune,412,yes\n"Gödel, Escher, Bach",777,no\nUntitled,,\n';
    expect(planSheet(csv).fields.map((f) => [f.name, f.type])).toEqual([
      ['title', 'text'],
      ['pageCount', 'number'],
      ['read', 'bool'],
    ]);
    const name = await addSpreadsheet(moul, { name: 'Books.csv', text: async () => csv });
    expect(name).toBe('books');
    const rows = await moul.collection(name).all({ sort: 'title' });
    expect(rows.map((r) => [r.title, r.pageCount, r.read])).toEqual([
      ['Dune', 412, true],
      ['Gödel, Escher, Bach', 777, false],
      ['Untitled', null, null],
    ]);
    // The same file again makes a second collection rather than mixing rows.
    expect(await addSpreadsheet(moul, { name: 'Books.csv', text: async () => csv })).toBe('books_2');
  });

  it('do not collide with a collection whose name differs only in case', async () => {
    const moul = await createHeadlessClient({ owner: true });
    await moul.admin.createCollection({ name: 'Tasks', fields: [{ name: 'title', type: 'text' }] });
    const name = await addSpreadsheet(moul, { name: 'tasks.csv', text: async () => 'Title\nWater the plants\n' });
    expect(name).toBe('tasks_2');
    expect((await moul.collection(name).all()).map((r) => r.title)).toEqual(['Water the plants']);
  });
});

describe('numbers that a double cannot hold', () => {
  it('keep their column as text, decimals and exponents included', async () => {
    const { columnType } = await import('../src/spreadsheet');
    expect(columnType(['9007199254740993.0'])).toBe('text');
    expect(columnType(['4111111111111111111.00'])).toBe('text');
    expect(columnType(['0.12345678901234567'])).toBe('text');
    expect(columnType(['1.5', '2.25', '1e3', '1000000000000000000000'])).toBe('text');
    expect(columnType(['1.5', '2.25', '1e3', '123456789012345'])).toBe('number');
  });
});

describe('the sample app', () => {
  it('loads, and its rules keep each person to their own tasks', async () => {
    const owner = await createHeadlessClient({ owner: true });
    await loadSample(owner);
    expect((await owner.collection('tasks').all()).length).toBe(3);
    const { MoulClient } = await import('@moul-dev/client');
    const app = new MoulClient(owner.transport, { authStorage: null });
    expect((await app.collection('tasks').list()).totalItems).toBe(0);
    await app.auth('users').signInWithPassword('ana@example.com', 'Password123!');
    expect((await app.collection('tasks').list()).totalItems).toBe(3);
  });
});
