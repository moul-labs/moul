/** Why a request failed, when it is not an answer from the engine. */
export type MoulErrorCode =
  /** The tab that held the database closed before answering. The request may or may not have run. */
  | 'leader-lost'
  /** The server could not be reached. */
  | 'network'
  /** The engine did not start in this page. */
  | 'engine'
  /** The client was closed. */
  | 'closed'
  /** The request asks for something this kind of connection does not offer, such as sync settings over HTTP. */
  | 'unsupported';

/**
 * A failed request. `status` is the engine's HTTP status (0 when the request never got an answer),
 * `data` its JSON body, and `message` the engine's message.
 */
export class MoulError extends Error {
  readonly status: number;
  readonly data: any;
  readonly code: MoulErrorCode | undefined;

  constructor(status: number, message: string, data?: any, code?: MoulErrorCode) {
    super(message);
    this.name = 'MoulError';
    this.status = status;
    this.data = data;
    this.code = code;
  }

  /** Whether the engine refused the request because a request budget ran out. */
  get budget(): string | undefined {
    return typeof this.data?.budget === 'string' ? this.data.budget : undefined;
  }
}

export const LEADER_LOST = 'the leading tab went away before answering';

/** Turns whatever a transport threw into a `MoulError`. */
export function toMoulError(err: unknown): MoulError {
  if (err instanceof MoulError) return err;
  const message = err instanceof Error ? err.message : String(err);
  if (message.includes(LEADER_LOST)) return new MoulError(0, message, undefined, 'leader-lost');
  return new MoulError(0, message, undefined, 'engine');
}
