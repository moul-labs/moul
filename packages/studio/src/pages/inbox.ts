import type { InboxMessage } from '@moul-dev/client';
import { css, html, LitElement, nothing } from 'lit';
import { repeat } from 'lit/directives/repeat.js';
import { copyText } from '../components/records/record-helpers';
import { MoulLiveController } from '../lib/moul-live';
import { onPress } from '../lib/press';
import { toast } from '../lib/toast';
import { moul } from '../studio';
import { baseStyles } from '../styles/base';
import { pageHeader, pageStyles } from '../styles/page';

/** A six-digit sign-in code in a message, if there is one. */
const codeOf = (m: InboxMessage) => /\b(\d{6})\b/.exec(m.body)?.[1];
/** Links in a message, such as a password reset link. */
const linksOf = (m: InboxMessage) => [...new Set(m.body.match(/https?:\/\/[^\s"'<>]+/g) ?? [])];

/**
 * The local inbox: mail the engine would send (sign-in codes, reset links). A tab cannot send
 * mail, so the codes land here, and the builder signs in to their own app with them.
 */
export class AdminInboxPage extends LitElement {
  static override styles = [
    baseStyles,
    pageStyles,
    css`
      .list {
        display: flex;
        flex-direction: column;
        gap: var(--spacing-3);
      }
      .meta {
        display: flex;
        flex-wrap: wrap;
        align-items: center;
        gap: var(--spacing-2);
        font-size: var(--font-size-sm);
        color: var(--color-fg-subtle);
      }
      .subject {
        font-weight: 600;
        color: var(--color-fg);
      }
      .code {
        display: inline-flex;
        align-items: center;
        gap: var(--spacing-2);
        font-family: var(--font-mono);
        font-size: 1.5rem;
        letter-spacing: 0.2em;
        margin: var(--spacing-2) 0;
      }
      pre {
        margin: var(--spacing-2) 0 0;
        white-space: pre-wrap;
        word-break: break-word;
        font-family: var(--font-mono);
        font-size: var(--font-size-xs, 0.75rem);
        color: var(--color-fg-subtle);
      }
      a {
        color: var(--color-primary-600);
        word-break: break-all;
      }
    `,
  ];

  private readonly inbox = new MoulLiveController(this, () => moul().admin.liveInbox());

  private async copy(text: string) {
    if (await copyText(text)) toast.success('Copied');
  }

  private renderMessage(m: InboxMessage) {
    const code = codeOf(m);
    const links = linksOf(m);
    return html`<moul-card>
      <moul-card-body>
        <div class="meta">
          <moul-badge size="sm" tone=${m.kind === 'otp' ? 'primary' : 'neutral'}>${m.kind}</moul-badge>
          <span class="subject">${m.subject}</span>
          <span>to ${m.to}</span>
          <span>· ${m.collection}</span>
          <span>· ${new Date(m.createdAt).toLocaleString()}</span>
        </div>
        ${code
          ? html`<div class="code">
              ${code}
              <moul-button size="sm" variant="ghost" is-icon aria-label="Copy the code" @click=${onPress(() => this.copy(code))}>
                <admin-icon name="copy" size="14"></admin-icon>
              </moul-button>
            </div>`
          : nothing}
        ${links.map((l) => html`<div><a href=${l} target="_blank" rel="noopener noreferrer">${l}</a></div>`)}
        <pre>${m.body}</pre>
      </moul-card-body>
    </moul-card>`;
  }

  override render() {
    const { data, loading, error } = this.inbox.state;
    return html`
      ${pageHeader('Inbox', 'Sign-in codes and reset links. A tab cannot send mail, so they arrive here instead.')}
      ${error ? html`<moul-alert tone="error" description=${error.message}></moul-alert>` : nothing}
      ${loading
        ? html`<div class="muted">Loading…</div>`
        : !data?.length
          ? html`<moul-empty-state variant="bordered">
              <admin-icon slot="icon" name="envelope-simple" size="32" color="var(--color-primary-500)"></admin-icon>
              <span slot="title">No mail yet</span>
              <span slot="description"
                >When your app asks for a sign-in code or a password reset, the message shows up here.</span
              >
            </moul-empty-state>`
          : html`<div class="list">${repeat(data, (m) => m.id, (m) => this.renderMessage(m))}</div>`}
    `;
  }
}

customElements.define('admin-inbox-page', AdminInboxPage);

declare global {
  interface HTMLElementTagNameMap {
    'admin-inbox-page': AdminInboxPage;
  }
}
