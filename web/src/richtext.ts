/**
 * Message body rendering: a small, deliberately-limited markdown dialect.
 *
 * Supported, because these are what people actually type in chat:
 * `**bold**`, `*italic*`, `~~strike~~`, `` `code` ``, ```` ```blocks``` ````,
 * `> quotes`, bare URLs, `@mentions`, and `#channel` references.
 *
 * **Everything is produced as DOM nodes.** There is no HTML string anywhere in
 * this file, so a message body cannot introduce an element — the worst a
 * malicious message can do is look odd. This is why the app needs no sanitizer
 * and why the CSP has no `unsafe-inline`.
 */

import { el } from './dom.ts';
import { HL_END, HL_START } from './protocol.ts';
import type { Id, User } from './protocol.ts';

export type RenderContext = {
  /** Resolve a handle to a user, for highlighting real mentions. */
  userByHandle: (handle: string) => User | undefined;
  /** Resolve a channel name to an id, for `#channel` links. */
  channelByName: (name: string) => Id | undefined;
  /** The viewing user's id, so mentions of them can be styled differently. */
  meId?: Id;
  onMention?: (userId: Id) => void;
  onChannel?: (channelId: Id) => void;
};

/** Render a message body into a fragment. */
export function renderBody(body: string, ctx: RenderContext): DocumentFragment {
  const frag = document.createDocumentFragment();
  for (const block of splitBlocks(body)) {
    if (block.type === 'code') {
      const pre = el('pre', { class: 'code-block' });
      // `textContent` — a code block full of markup stays text.
      pre.appendChild(el('code', { text: block.text, ...(block.lang ? { 'data-lang': block.lang } : {}) }));
      frag.appendChild(pre);
    } else if (block.type === 'quote') {
      const q = el('blockquote', { class: 'quote' });
      q.appendChild(renderInline(block.text, ctx));
      frag.appendChild(q);
    } else {
      const p = el('p', { class: 'para' });
      p.appendChild(renderInline(block.text, ctx));
      frag.appendChild(p);
    }
  }
  return frag;
}

type Block =
  | { type: 'text'; text: string }
  | { type: 'quote'; text: string }
  | { type: 'code'; text: string; lang: string };

/** Split into fenced code blocks, quotes, and paragraphs. */
function splitBlocks(body: string): Block[] {
  const blocks: Block[] = [];
  const lines = body.split('\n');
  let buffer: string[] = [];
  let mode: 'text' | 'quote' = 'text';

  const flush = () => {
    if (buffer.length) {
      blocks.push({ type: mode, text: buffer.join('\n') });
      buffer = [];
    }
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    if (line.startsWith('```')) {
      flush();
      const lang = line.slice(3).trim();
      const code: string[] = [];
      i++;
      while (i < lines.length && !lines[i].startsWith('```')) {
        code.push(lines[i]);
        i++;
      }
      blocks.push({ type: 'code', text: code.join('\n'), lang });
      continue;
    }

    const isQuote = line.startsWith('> ') || line === '>';
    const lineMode = isQuote ? 'quote' : 'text';
    if (lineMode !== mode) {
      flush();
      mode = lineMode;
    }
    buffer.push(isQuote ? line.replace(/^>\s?/, '') : line);
  }
  flush();
  return blocks;
}

/** Inline token patterns, tried in order at each position. */
const INLINE = [
  { re: /^`([^`\n]+)`/, kind: 'code' },
  { re: /^\*\*([^*\n]+)\*\*/, kind: 'bold' },
  { re: /^__([^_\n]+)__/, kind: 'bold' },
  { re: /^~~([^~\n]+)~~/, kind: 'strike' },
  { re: /^\*([^*\n]+)\*/, kind: 'italic' },
  { re: /^_([^_\n]+)_/, kind: 'italic' },
] as const;

const URL_RE = /^https?:\/\/[^\s<>"')\]]+/;
const MENTION_RE = /^@([a-z0-9._-]+)/i;
const CHANNEL_RE = /^#([a-z0-9_-]+)/i;

function renderInline(text: string, ctx: RenderContext): DocumentFragment {
  const frag = document.createDocumentFragment();
  let plain = '';
  let i = 0;

  const flushPlain = () => {
    if (plain) {
      frag.appendChild(document.createTextNode(plain));
      plain = '';
    }
  };

  while (i < text.length) {
    const rest = text.slice(i);
    const ch = text[i];

    // Inline markers only start at a boundary, so `a*b*c` and snake_case
    // identifiers are left alone.
    const atBoundary = i === 0 || /[\s(["'‘“]/.test(text[i - 1]);

    let matched = false;
    if (atBoundary) {
      for (const { re, kind } of INLINE) {
        const m = re.exec(rest);
        if (!m) continue;
        flushPlain();
        if (kind === 'code') {
          frag.appendChild(el('code', { class: 'inline-code', text: m[1] }));
        } else {
          const tag = kind === 'bold' ? 'strong' : kind === 'italic' ? 'em' : 's';
          frag.appendChild(el(tag, { text: m[1] }));
        }
        i += m[0].length;
        matched = true;
        break;
      }
      if (matched) continue;

      if (ch === 'h') {
        const m = URL_RE.exec(rest);
        if (m) {
          flushPlain();
          // Trailing sentence punctuation belongs to the sentence.
          const url = m[0].replace(/[.,;:!?]+$/, '');
          frag.appendChild(
            el('a', {
              class: 'link',
              href: url,
              text: url,
              // `noopener` severs `window.opener`; `noreferrer` keeps our URLs
              // out of other sites' referer logs.
              rel: 'noopener noreferrer nofollow',
              target: '_blank',
            }),
          );
          i += url.length;
          continue;
        }
      }

      if (ch === '@') {
        const m = MENTION_RE.exec(rest);
        if (m) {
          const handle = m[1].toLowerCase().replace(/[.\-_]+$/, '');
          const special = handle === 'here' || handle === 'channel' || handle === 'everyone';
          const user = special ? undefined : ctx.userByHandle(handle);
          if (user || special) {
            flushPlain();
            const isMe = special || (!!ctx.meId && user?.id === ctx.meId);
            const span = el('span', {
              class: `mention${isMe ? ' mention-me' : ''}`,
              text: `@${user ? user.n || user.h : handle}`,
              role: user ? 'button' : undefined,
              tabIndex: user ? 0 : undefined,
            });
            if (user && ctx.onMention) {
              span.addEventListener('click', () => ctx.onMention?.(user.id));
            }
            frag.appendChild(span);
            i += 1 + handle.length;
            continue;
          }
        }
      }

      if (ch === '#') {
        const m = CHANNEL_RE.exec(rest);
        if (m) {
          const id = ctx.channelByName(m[1].toLowerCase());
          if (id) {
            flushPlain();
            const span = el('span', {
              class: 'channel-ref',
              text: `#${m[1]}`,
              role: 'button',
              tabIndex: 0,
            });
            if (ctx.onChannel) span.addEventListener('click', () => ctx.onChannel?.(id));
            frag.appendChild(span);
            i += m[0].length;
            continue;
          }
        }
      }
    }

    plain += ch;
    i++;
  }
  flushPlain();
  return frag;
}

/**
 * Render a search snippet, converting the server's highlight sentinels into
 * elements.
 *
 * The order matters: the snippet arrives as plain text with U+0002/U+0003
 * markers, and each run becomes a text node or a `<mark>`. Because nothing is
 * ever parsed as HTML, a message containing `<mark>` renders it literally
 * instead of injecting one.
 */
export function renderSnippet(snippet: string): DocumentFragment {
  const frag = document.createDocumentFragment();
  let i = 0;
  while (i < snippet.length) {
    const start = snippet.indexOf(HL_START, i);
    if (start === -1) {
      frag.appendChild(document.createTextNode(snippet.slice(i)));
      break;
    }
    if (start > i) frag.appendChild(document.createTextNode(snippet.slice(i, start)));
    const end = snippet.indexOf(HL_END, start);
    if (end === -1) {
      frag.appendChild(document.createTextNode(snippet.slice(start + 1)));
      break;
    }
    frag.appendChild(el('mark', { text: snippet.slice(start + 1, end) }));
    i = end + 1;
  }
  return frag;
}

/** True when a body is nothing but emoji, which the UI renders larger. */
export function isEmojiOnly(body: string): boolean {
  const trimmed = body.trim();
  if (!trimmed || trimmed.length > 24) return false;
  // \p{Extended_Pictographic} covers emoji; the rest allows joiners, variation
  // selectors, skin-tone modifiers, and spaces between them.
  return /^(?:\p{Extended_Pictographic}|\p{Emoji_Component}|[‍️\s])+$/u.test(trimmed);
}
