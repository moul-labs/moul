import type { LiveQuery, LiveState } from '@moul-dev/client';
import type { ReactiveController, ReactiveControllerHost } from 'lit';

const sameDeps = (a: readonly unknown[], b: readonly unknown[]) =>
  a.length === b.length && a.every((value, i) => Object.is(value, b[i]));

/**
 * Follows one of the SDK's live queries while the host is connected. `make` returns the query, or
 * nothing to follow none; it is made again whenever a value from `deps` changes.
 *
 *   private inbox = new MoulLiveController(this, () => moul().admin.liveInbox());
 *   render() { const { data, loading } = this.inbox.state; … }
 */
export class MoulLiveController<T> implements ReactiveController {
  state: LiveState<T> = { data: undefined, error: undefined, loading: true };
  private stop: (() => void) | undefined;
  private lastDeps: readonly unknown[] | undefined;
  private readonly host: ReactiveControllerHost;
  private readonly make: () => LiveQuery<T> | undefined;
  private readonly deps: () => readonly unknown[];

  constructor(host: ReactiveControllerHost, make: () => LiveQuery<T> | undefined, deps: () => readonly unknown[] = () => []) {
    this.host = host;
    this.make = make;
    this.deps = deps;
    host.addController(this);
  }

  hostConnected(): void {
    this.follow();
  }

  hostUpdate(): void {
    if (this.lastDeps && !sameDeps(this.lastDeps, this.deps())) this.follow();
  }

  hostDisconnected(): void {
    this.stop?.();
    this.stop = undefined;
    this.lastDeps = undefined;
  }

  /** Fetches again now. */
  refresh(): void {
    this.follow();
  }

  private follow(): void {
    this.stop?.();
    this.lastDeps = this.deps();
    const query = this.make();
    if (!query) {
      this.state = { data: undefined, error: undefined, loading: false };
      this.host.requestUpdate();
      return;
    }
    this.stop = query.subscribe((state) => {
      this.state = state;
      this.host.requestUpdate();
    });
  }
}
