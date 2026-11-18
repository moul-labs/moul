import { css, html, LitElement, nothing } from 'lit';
import { api } from '../../api/client';
import { fieldValue } from '../../lib/forms';
import { onPress } from '../../lib/press';
import { baseStyles } from '../../styles/base';
import type { MoulRules } from './rules-editor';

const LABELS: Record<keyof MoulRules, string> = {
  listRule: 'List',
  viewRule: 'View',
  createRule: 'Create',
  updateRule: 'Update',
  deleteRule: 'Delete',
};

type Result = { valid: boolean; matched: boolean; translated: string; error?: string };

function parse(text: string, what: string): object | null {
  if (!text.trim()) return null;
  try {
    const v = JSON.parse(text);
    if (v && typeof v === 'object' && !Array.isArray(v)) return v;
  } catch {
    // reported below
  }
  throw new Error(`${what} must be a JSON object`);
}

/**
 * Tries one of a collection's rules (as edited, before saving) against a sample record, a
 * signed-in person and a request body, with the engine's own evaluator.
 */
export class AdminRuleTester extends LitElement {
  static override styles = [
    baseStyles,
    css`
      :host {
        display: block;
      }
      .box {
        display: flex;
        flex-direction: column;
        gap: var(--spacing-3);
        padding: var(--spacing-4);
        border: 1px solid var(--color-border);
        border-radius: var(--radius-lg, 12px);
      }
      h4 {
        margin: 0;
        font-size: var(--font-size-sm);
      }
      .rules {
        display: flex;
        flex-wrap: wrap;
        gap: var(--spacing-1);
      }
      .grid {
        display: grid;
        grid-template-columns: repeat(auto-fit, minmax(min(14rem, 100%), 1fr));
        gap: var(--spacing-3);
      }
      .result {
        font-size: var(--font-size-sm);
      }
      code {
        font-family: var(--font-mono);
        font-size: 0.75rem;
        word-break: break-word;
      }
    `,
  ];

  static override properties = {
    rules: { attribute: false },
    which: { state: true },
    record: { state: true },
    auth: { state: true },
    body: { state: true },
    result: { state: true },
    error: { state: true },
  };

  declare rules: MoulRules;
  declare which: keyof MoulRules;
  declare record: string;
  declare auth: string;
  declare body: string;
  declare result: Result | undefined;
  declare error: string | undefined;

  constructor() {
    super();
    this.rules = {};
    this.which = 'listRule';
    this.record = '{\n  "owner": "u1"\n}';
    this.auth = '{\n  "id": "u1"\n}';
    this.body = '';
  }

  private async run() {
    this.error = undefined;
    this.result = undefined;
    try {
      const rule = this.rules[this.which] ?? '';
      const record = parse(this.record, 'The record') ?? {};
      const auth = parse(this.auth, 'The person');
      const body = parse(this.body, 'The request body');
      this.result = await api.testRule({ rule, record, auth, request: body ? { body } : undefined });
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
    }
  }

  override render() {
    const rule = this.rules[this.which] ?? '';
    const r = this.result;
    return html`<div class="box">
      <h4>Try a rule</h4>
      <div class="rules">
        ${(Object.keys(LABELS) as (keyof MoulRules)[]).map(
          (k) => html`<moul-button size="sm" variant=${k === this.which ? 'primary' : 'outline'} @click=${onPress(() => {
            this.which = k;
            this.result = undefined;
          })}>${LABELS[k]}</moul-button>`,
        )}
      </div>
      <div class="result"><code>${rule || '(empty: everyone)'}</code></div>
      <div class="grid">
        <moul-text-area label="Record" rows="4" .value=${this.record} @input=${(e: Event) => (this.record = fieldValue(e))}></moul-text-area>
        <moul-text-area
          label="Signed-in person"
          description="Empty for a guest"
          rows="4"
          .value=${this.auth}
          @input=${(e: Event) => (this.auth = fieldValue(e))}
        ></moul-text-area>
        <moul-text-area
          label="Request body"
          description="What a create or update sends"
          rows="4"
          .value=${this.body}
          @input=${(e: Event) => (this.body = fieldValue(e))}
        ></moul-text-area>
      </div>
      <div><moul-button variant="secondary" @click=${onPress(() => this.run())}>Test</moul-button></div>
      ${this.error ? html`<moul-alert tone="error" description=${this.error}></moul-alert>` : nothing}
      ${r
        ? html`<moul-alert
            tone=${!r.valid ? 'error' : r.matched ? 'success' : 'warning'}
            description=${!r.valid ? (r.error ?? 'The rule does not read.') : r.matched ? 'Allowed.' : 'Refused.'}
          ></moul-alert>`
        : nothing}
    </div>`;
  }
}

customElements.define('admin-rule-tester', AdminRuleTester);

declare global {
  interface HTMLElementTagNameMap {
    'admin-rule-tester': AdminRuleTester;
  }
}
