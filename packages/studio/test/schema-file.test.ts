import { describe, expect, it } from 'vitest';
import { collectionsPrunedBy } from '../src/lib/schema-file';

const stored = [
  { id: 'c1', name: 'tasks' },
  { id: 'c2', name: 'notes' },
  { id: 'c3', name: 'users' },
];

describe('collections a pruning schema file deletes', () => {
  it('are those it lists neither by ID nor, without an ID, by name', () => {
    const file = { collections: [{ name: ' tasks ' }, { id: 'c3', name: 'people' }] };
    expect(collectionsPrunedBy(file, stored)).toEqual(['notes']);
  });

  it('match names exactly, as the engine does', () => {
    expect(collectionsPrunedBy({ collections: [{ name: 'Tasks' }] }, stored)).toEqual(['tasks', 'notes', 'users']);
  });

  it('include a collection whose name an entry with another ID takes', () => {
    expect(collectionsPrunedBy({ collections: [{ id: 'other', name: 'tasks' }] }, stored)).toEqual(['tasks', 'notes', 'users']);
  });

  it('are none for a file the engine refuses', () => {
    expect(collectionsPrunedBy({}, stored)).toEqual([]);
    expect(collectionsPrunedBy(null, stored)).toEqual([]);
  });
});
