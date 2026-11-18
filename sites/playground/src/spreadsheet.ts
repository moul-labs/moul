import type { MoulClient } from '@moul-dev/client';

// A spreadsheet (CSV) becomes a collection: one text, number or bool field per column, and one
// record per row.

/** Parses CSV text: commas, quotes ("" for a quote), and line breaks inside quotes. */
export function parseCsv(text: string): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  let cell = '';
  let quoted = false;
  const src = text.replace(/^﻿/, '');
  for (let i = 0; i < src.length; i++) {
    const c = src[i];
    if (quoted) {
      if (c === '"') {
        if (src[i + 1] === '"') {
          cell += '"';
          i++;
        } else quoted = false;
      } else cell += c;
    } else if (c === '"') quoted = true;
    else if (c === ',') {
      row.push(cell);
      cell = '';
    } else if (c === '\n' || c === '\r') {
      if (c === '\r' && src[i + 1] === '\n') i++;
      row.push(cell);
      rows.push(row);
      row = [];
      cell = '';
    } else cell += c;
  }
  if (cell !== '' || row.length) {
    row.push(cell);
    rows.push(row);
  }
  return rows.filter((r) => r.some((c) => c.trim() !== ''));
}

/**
 * A field name from a column heading, as the engine takes them: camelCase, starting with a
 * lowercase letter, letters and digits only (`Due date` → `dueDate`).
 */
export function fieldName(heading: string, taken: Set<string>): string {
  const words = heading
    .normalize('NFKD')
    .replace(/[\u0300-\u036f]/g, '')
    .split(/[^A-Za-z0-9]+/)
    .filter(Boolean);
  let name = words.map((w, i) => (i === 0 ? w.toLowerCase() : w[0].toUpperCase() + w.slice(1).toLowerCase())).join('');
  if (!name) name = 'column';
  if (!/^[a-z]/.test(name)) name = `c${name[0].toUpperCase()}${name.slice(1)}`;
  // Names the engine gives every record.
  if (['id', 'createdat', 'updatedat', 'expand'].includes(name.toLowerCase())) name = `${name}Field`;
  let out = name;
  for (let i = 2; taken.has(out.toLowerCase()); i++) out = `${name}${i}`;
  taken.add(out.toLowerCase());
  return out;
}

export type ColumnType = 'text' | 'number' | 'bool';

// No leading zeros: `01234` is a code (a zip code, an account number), not 1234.
const NUMBER = /^-?(0|[1-9]\d*)(\.\d+)?([eE][-+]?\d+)?$/;
const BOOL = /^(true|false|yes|no)$/i;

/**
 * Whether `value` reads as a number without losing anything: written without leading zeros, finite
 * as a double, a whole number no larger than a double holds exactly, and with a point or an
 * exponent no more significant digits than a double keeps (15).
 */
function isNumber(value: string): boolean {
  if (!NUMBER.test(value)) return false;
  const n = Number(value);
  if (!Number.isFinite(n)) return false;
  // A plain whole number is exact up to Number.MAX_SAFE_INTEGER.
  if (!/[.eE]/.test(value)) return Number.isSafeInteger(n);
  // Written with a point or an exponent: a double keeps 15 significant digits for sure.
  const digits = value
    .replace(/^-/, '')
    .replace(/[eE].*$/, '')
    .replace('.', '')
    .replace(/^0+/, '')
    .replace(/0+$/, '');
  if (digits.length > 15) return false;
  return !Number.isInteger(n) || Number.isSafeInteger(n);
}

/** The field type that fits every non-empty value of a column. */
export function columnType(values: string[]): ColumnType {
  const filled = values.map((v) => v.trim()).filter((v) => v !== '');
  if (filled.length === 0) return 'text';
  if (filled.every(isNumber)) return 'number';
  if (filled.every((v) => BOOL.test(v))) return 'bool';
  return 'text';
}

export interface SheetPlan {
  fields: { name: string; type: ColumnType; heading: string }[];
  records: Record<string, string | number | boolean | null>[];
}

/** The collection fields and records a CSV file makes. */
export function planSheet(text: string): SheetPlan {
  const rows = parseCsv(text);
  if (rows.length === 0) throw new Error('the file has no rows');
  const [head, ...body] = rows;
  const taken = new Set<string>();
  const fields = head.map((heading, i) => ({
    heading,
    name: fieldName(heading, taken),
    type: columnType(body.map((r) => r[i] ?? '')),
  }));
  const records = body.map((r) => {
    const rec: SheetPlan['records'][number] = {};
    fields.forEach((f, i) => {
      const raw = (r[i] ?? '').trim();
      if (raw === '') rec[f.name] = null;
      else if (f.type === 'number') rec[f.name] = Number(raw);
      else if (f.type === 'bool') rec[f.name] = /^(true|yes)$/i.test(raw);
      else rec[f.name] = r[i] ?? '';
    });
    return rec;
  });
  return { fields, records };
}

/** A collection name from a file name. SQLite keeps names starting `sqlite_` for itself. */
export function collectionName(file: string): string {
  const base = file
    .replace(/\.[^.]+$/, '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '_')
    .replace(/^_+|_+$/g, '');
  return (/^[a-z]/.test(base) && !base.startsWith('sqlite_') ? base : `sheet_${base || 'data'}`).slice(0, 56);
}

/** Adds a spreadsheet to `client`'s database as a new collection. Returns its name. */
export async function addSpreadsheet(client: MoulClient, file: { name: string; text(): Promise<string> }): Promise<string> {
  const plan = planSheet(await file.text());
  const owner = client.asOwner();
  // The engine compares collection names ignoring case: `Tasks` takes `tasks`.
  const existing = new Set((await owner.admin.listCollections()).map((c) => c.name.toLowerCase()));
  let name = collectionName(file.name);
  for (let i = 2; existing.has(name); i++) name = `${collectionName(file.name)}_${i}`;
  await owner.admin.createCollection({
    name,
    fields: plan.fields.map((f) => ({ name: f.name, type: f.type })),
    // Open to anyone while you try it out; set rules in Studio before others use it.
    rules: { listRule: '', viewRule: '', createRule: '', updateRule: '', deleteRule: '' },
  });
  await owner.collection(name).import(JSON.stringify(plan.records), 'application/json');
  return name;
}

