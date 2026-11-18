import type { LogItem } from '@moul-dev/web-component';
import { css, html, LitElement, type PropertyValues } from 'lit';
import { repeat } from 'lit/directives/repeat.js';
import { moulsCollection } from '../api/db';
import { moul } from '../studio';
import { LiveQueryController } from '../lib/live-query';
import { onPress } from '../lib/press';
import { baseStyles } from '../styles/base';
import { pageHeader, pageStyles } from '../styles/page';

const MAX_EVENTS = 200;

function logLevelForAction(action: string): 'info' | 'warn' | 'debug' {
  switch (action) {
    case 'delete':
      return 'warn';
    case 'create':
      return 'info';
    default:
      return 'debug';
  }
}

const newEventId = () => `${Date.now()}-${Math.random().toString(36).slice(2, 7)}`;

/** Live record events from the engine, for one collection or all of them. */
export class AdminRealtimePage extends LitElement {
  static override styles = [
    baseStyles,
    pageStyles,
    css`
      :host {
        --page-gap: var(--spacing-4);
      }

      .badge-content {
        display: inline-flex;
        align-items: center;
        gap: var(--spacing-1);
      }

      .toolbar {
        display: flex;
        align-items: center;
        justify-content: space-between;
        flex-wrap: wrap;
        gap: var(--spacing-3);
        width: 100%;
      }

      .controls {
        display: flex;
        align-items: center;
        flex-wrap: wrap;
        gap: var(--spacing-3);
      }

      moul-select {
        width: 280px;
        max-width: 100%;
      }
    `,
  ];

  static override properties = {
    selectedMoul: { state: true },
    isStreaming: { state: true },
    events: { state: true },
  };

  declare selectedMoul: string;
  declare isStreaming: boolean;
  declare events: LogItem[];

  private readonly mouls = new LiveQueryController(this, (q) => q.from({ moul: moulsCollection }));

  private unsubscribe?: () => void;
  /** Bumped on every resubscribe and on leaving the page, so a subscription that resolves late is dropped. */
  private generation = 0;

  constructor() {
    super();
    this.selectedMoul = '*';
    this.isStreaming = true;
    this.events = [];
  }

  override connectedCallback(): void {
    super.connectedCallback();
    // Re-opens the stream when the page is shown again after navigating away.
    if (this.hasUpdated) this.resubscribe();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    this.unsubscribe?.();
    this.unsubscribe = undefined;
    this.generation++;
  }

  private followed = '';

  override updated(changed: PropertyValues<this>): void {
    // "All collections" follows each one, so a new collection needs a new subscription.
    const names = this.mouls.data.map((m) => m.name).join(',');
    if (changed.has('selectedMoul') || changed.has('isStreaming') || (this.selectedMoul === '*' && names !== this.followed)) {
      this.followed = names;
      this.resubscribe();
    }
  }

  private pushEvent(item: LogItem) {
    this.events = [item, ...this.events].slice(0, MAX_EVENTS);
  }

  private resubscribe() {
    this.unsubscribe?.();
    this.unsubscribe = undefined;
    const generation = ++this.generation;
    if (!this.isStreaming) return;

    const selected = this.selectedMoul;
    const names = selected === '*' ? this.mouls.data.map((m) => m.name) : [selected];
    const stops: (() => void)[] = [];
    // Aborting ends the subscriptions still being made, too.
    const abort = new AbortController();
    this.unsubscribe = () => {
      abort.abort();
      for (const stop of stops) stop();
    };
    for (const name of names) {
      moul()
        .collection(name)
        .subscribe((event) => {
          const action = event.action || 'mutation';
          const record = event.record as { id?: unknown } | undefined;
          const recordId = record?.id ? String(record.id) : '';
          this.pushEvent({
            id: newEventId(),
            timestamp: new Date().toLocaleTimeString(),
            level: logLevelForAction(action),
            message: `[${event.moul || name}] ${action.toUpperCase()} record: ${recordId || 'new'}`,
            attributes: { collection: event.moul || name, action, recordId: recordId || undefined },
            raw: JSON.stringify(event.old_record ? { record: event.record, old: event.old_record } : event.record, null, 2),
          });
        }, { signal: abort.signal })
        .then(
          (stop) => {
            if (generation === this.generation) stops.push(stop);
            else stop();
          },
          (err: Error) => {
            if (generation !== this.generation) return;
            this.pushEvent({
              id: newEventId(),
              timestamp: new Date().toLocaleTimeString(),
              level: 'error',
              message: `Could not follow ${name}: ${err.message}`,
              raw: String(err),
            });
          },
        );
    }
  }

  override render() {
    const mouls = this.mouls.data;

    return html`
      ${pageHeader(
        'Realtime',
        'Record events as they happen, from this tab, other tabs and your other devices.',
        html`
          <moul-badge tone=${this.isStreaming ? 'success' : 'error'}>
            <span class="badge-content">
              <admin-icon name="broadcast" weight="bold" size="14"></admin-icon>
              ${this.isStreaming ? 'Following' : 'Paused'}
            </span>
          </moul-badge>
        `,
      )}

      <moul-card>
        <moul-card-body>
          <div class="toolbar">
            <div class="controls">
              <!-- A select, not the React page's combobox: <moul-combobox> filters its list by the
                   selected item's text, so reopening it would show only that one collection. -->
              <moul-select
                aria-label="Select Collection"
                placeholder="Select Collection"
                selected-key=${this.selectedMoul}
                @selectionchange=${(e: CustomEvent<{ selectedKey: string }>) => {
                  if (e.detail.selectedKey) this.selectedMoul = e.detail.selectedKey;
                }}
              >
                <moul-select-item key="*">All collections</moul-select-item>
                ${repeat(
                  mouls,
                  (m) => m.name,
                  (m) => html`<moul-select-item key=${m.name}>${m.name}</moul-select-item>`,
                )}
              </moul-select>
              <moul-button variant=${this.isStreaming ? 'danger' : 'primary'} @click=${onPress(() => (this.isStreaming = !this.isStreaming))}>
                <admin-icon slot="prefix" name=${this.isStreaming ? 'stop' : 'play'} size="14"></admin-icon>
                ${this.isStreaming ? 'Pause' : 'Follow'}
              </moul-button>
            </div>
            <moul-button variant="ghost" aria-label="Clear Stream Logs" @click=${onPress(() => (this.events = []))}>
              <admin-icon slot="prefix" name="trash" size="14"></admin-icon>
              Clear (${this.events.length})
            </moul-button>
          </div>
        </moul-card-body>
      </moul-card>

      <moul-logs-viewer
        title="Event Stream"
        max-height="640px"
        search-placeholder="Filter events or record ID..."
        .logs=${this.events}
        @clear=${() => (this.events = [])}
      ></moul-logs-viewer>
    `;
  }
}

customElements.define('admin-realtime-page', AdminRealtimePage);

declare global {
  interface HTMLElementTagNameMap {
    'admin-realtime-page': AdminRealtimePage;
  }
}
