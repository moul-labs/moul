/** Any JSON value. */
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

/** Query parameters: an object (undefined values are left out), pairs, or a query string. */
export type QueryInit =
  | Record<string, string | number | boolean | null | undefined>
  | [string, string][]
  | string;

/** A file in a request body. */
export interface FormFile {
  name: string;
  type: string;
  data: Uint8Array;
}

/**
 * A request to the engine. It has the shape of an HTTP request, because the same requests go to an
 * engine in the page, to a native server and to Moul Hub.
 */
export interface EngineRequest {
  /** `GET` unless set. */
  method?: string;
  /** Such as `/api/moul/tasks/records`. May carry a query string. */
  path: string;
  query?: QueryInit;
  headers?: Record<string, string>;
  /** A JSON body. */
  json?: unknown;
  /** A form body with files, as `multipart/form-data` carries it. */
  form?: { fields?: [string, string][]; files?: [string, FormFile][] };
  /** A raw body, with its media type in `type`. */
  bytes?: Uint8Array;
  type?: string;
  /** Sent as the database's owner: rules do not apply. Only a page that holds the database can claim it. */
  owner?: boolean;
  /** The request id of a write; a write sent twice with one id is applied once. */
  id?: string;
}

/** The engine's answer. */
export interface EngineResponse {
  status: number;
  headers: [string, string][];
  /** The body, when it is JSON. */
  json?: any;
  /** The body, when it is bytes (a file, a CSV export). */
  bytes?: Uint8Array;
  /** The media type of `bytes`. */
  type?: string;
}

/** One row of the change log: a write request the writer applied or refused. */
export interface Change {
  seq: number;
  request: string;
  /** `create`, `update`, `delete`, `define`, `undefine`, `schema`, `import`, `upload` or `unupload`. */
  op: string;
  /** The collection's id (ids survive renames). */
  collection: string | null;
  record: string | null;
  /** Ids of the fields that changed. */
  fields: string[];
  /** The HTTP status of the answer: below 400 applied, otherwise refused. */
  status: number;
  /** Other records the write changed: cascaded deletes and cleared references. */
  effects?: { collection: string; record: string; op: string }[];
}

/** What the engine announces after each commit. */
export interface ChangeNotice {
  type: 'changes';
  /** The database's commit number. */
  commit: number;
  /** New rows of the change log. */
  changes: Change[];
  /** The data changed in a way the log does not describe (raw SQL, an import, pages from another device). */
  all: boolean;
}

/** A record event, as subscriptions deliver it. */
export interface RecordEvent<T = RecordModel> {
  action: 'create' | 'update' | 'delete';
  /** The collection's name. */
  moul: string;
  record: T;
  /** The previous version, for updates. */
  old_record?: T;
  timestamp: string;
}

/** What every record has. */
export interface BaseRecord {
  id: string;
  createdAt: string;
  updatedAt: string;
}

/** A record of a collection the types do not describe. */
export interface RecordModel extends BaseRecord {
  [field: string]: any;
}

/** A page of records. */
export interface ListResult<T> {
  page: number;
  perPage: number;
  totalItems: number;
  totalPages: number;
  items: T[];
}

/** Options of a list request. */
export interface ListOptions {
  /** A filter in the rule language, such as `done = false && owner = @request.auth.id`. */
  filter?: string;
  /** Fields to sort by, comma-separated; `-` sorts descending: `-createdAt,title`. */
  sort?: string;
  /** Relation fields to expand, comma-separated. */
  expand?: string;
  /** Text to search for in the collection's text fields. */
  search?: string;
  /** 1-based. */
  page?: number;
  /** At most 500. */
  perPage?: number;
  /** Extra query parameters. */
  query?: Record<string, string>;
}

/** Options of a single-record request. */
export interface GetOptions {
  expand?: string;
}

/** A schema: collection name to record type, as `moul typegen` writes `MoulSchema`. */
export type AnySchema = Record<string, RecordModel>;

/** A file the engine stores, as uploads and file fields hold it. */
export interface MoulFile {
  id: string;
  url: string;
  filename?: string;
  size?: number;
  createdAt?: string;
}

/** A collection definition, as `/api/moul` returns it. */
export interface CollectionModel {
  id: string;
  name: string;
  type: 'base' | 'auth' | string;
  fields: FieldModel[];
  rules?: Partial<Record<'listRule' | 'viewRule' | 'createRule' | 'updateRule' | 'deleteRule' | 'subscribeRule', string>>;
  indexes?: string[];
  createdAt?: string;
  updatedAt?: string;
  [key: string]: unknown;
}

/** A field definition. */
export interface FieldModel {
  id?: string;
  name: string;
  type: string;
  required?: boolean;
  unique?: boolean;
  [key: string]: unknown;
}

/** A message the local inbox kept: sign-in codes and reset links, which a tab cannot email. */
export interface InboxMessage {
  id: string;
  collection: string;
  to: string;
  kind: string;
  subject: string;
  body: string;
  createdAt: string;
}
