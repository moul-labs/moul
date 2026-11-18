/**
 * `@moul-dev/client/headless`: the engine in memory, in this thread, without a browser. For tests
 * of an app's data, rules and sign-in in Node (or any JavaScript runtime with WebAssembly).
 *
 * ```ts
 * import { createHeadlessClient } from '@moul-dev/client/headless';
 * import schema from '../moul.schema.json';
 *
 * const moul = await createHeadlessClient({ schema, now: '2026-11-18T09:00:00Z', seed: 1 });
 * await moul.auth('users').signUp({ email: 'ana@example.com', password: 'Password123!' });
 * ```
 */

import { MoulClient, type MoulClientOptions } from './client.js';
import { EngineTransport, type InProcessEngine } from './transport.js';
import type { AnySchema } from './types.js';

export interface HeadlessOptions extends MoulClientOptions {
  /** A schema file to apply before the client is returned, as `moul schema` writes it. */
  schema?: object;
  /** Fix the clock at this time; move it with `advance`. The real clock otherwise. */
  now?: number | string | Date;
  /** Make ids, tokens and salts repeat from run to run. */
  seed?: number;
  /** The engine's `.wasm` file, when it is not next to the package. */
  wasm?: BufferSource | URL | string;
}

/** A headless client, with the engine's clock in reach. */
export interface HeadlessClient<S extends object = AnySchema> extends MoulClient<S> {
  /** Moves a fixed clock forward (see `now`). */
  advance(ms: number): void;
}

type WasmModule = {
  default: (input?: { module_or_path: unknown }) => Promise<unknown>;
  MoulEngine: new (options?: object) => InProcessEngine & { advance(ms: number): void };
};

let loaded: Promise<WasmModule> | undefined;

async function wasmBytes(): Promise<BufferSource | URL> {
  const url = new URL('../wasm/moul_web_bg.wasm', import.meta.url);
  const isNode = typeof (globalThis as any).process?.versions?.node === 'string';
  if (isNode && url.protocol === 'file:') {
    const { readFile } = await import(/* @vite-ignore */ 'node:fs/promises' as string);
    return (await readFile(url)) as Uint8Array as BufferSource;
  }
  return url;
}

async function load(wasm?: HeadlessOptions['wasm']): Promise<WasmModule> {
  loaded ??= (async () => {
    const mod = (await import('../wasm/moul_web.js')) as unknown as WasmModule;
    await mod.default({ module_or_path: wasm ?? (await wasmBytes()) });
    return mod;
  })();
  return loaded;
}

/** An engine in memory and a client on it. Each call makes a fresh, empty database. */
export async function createHeadlessClient<S extends object = AnySchema>(
  options: HeadlessOptions = {},
): Promise<HeadlessClient<S>> {
  const mod = await load(options.wasm);
  const now = options.now === undefined ? undefined : new Date(options.now).getTime();
  if (now !== undefined && Number.isNaN(now)) throw new TypeError(`now is not a date: ${String(options.now)}`);
  const engine = new mod.MoulEngine({ now, seed: options.seed });
  const client = new MoulClient<S>(new EngineTransport(engine), {
    authStorage: null,
    ...options,
  }) as HeadlessClient<S>;
  client.advance = (ms: number) => engine.advance(ms);
  // The schema in the options is applied before the first request; a test should fail loudly if
  // it does not apply.
  await client.ready;
  return client;
}

export { MoulClient } from './client.js';
export type * from './index.js';
