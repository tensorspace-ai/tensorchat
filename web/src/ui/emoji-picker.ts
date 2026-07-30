/**
 * The emoji picker popover.
 *
 * One instance is created lazily and reused, anchored to whatever button opened
 * it. Building a fresh grid of three hundred buttons on every open would be
 * visible work for no benefit — the contents never change.
 */

import { el, replace } from '../dom.ts';
import { allEmoji, categories, searchEmoji, type Emoji } from '../emoji.ts';

type OpenOptions = {
  /** The element to position against. */
  anchor: HTMLElement;
  onPick: (emoji: string) => void;
};

let singleton: Picker | null = null;

type Picker = {
  root: HTMLElement;
  open: (opts: OpenOptions) => void;
  close: () => void;
};

/** Open the picker against `anchor`. Creates it on first use. */
export function openEmojiPicker(opts: OpenOptions): void {
  singleton ??= createPicker();
  singleton.open(opts);
}

export function closeEmojiPicker(): void {
  singleton?.close();
}

function createPicker(): Picker {
  const search = el('input', {
    class: 'emoji-search',
    placeholder: 'Search emoji…',
    'aria-label': 'Search emoji',
  }) as HTMLInputElement;
  const grid = el('div', { class: 'emoji-grid' });
  const root = el(
    'div',
    { class: 'emoji-picker', hidden: true, role: 'dialog', aria: { label: 'Emoji' } },
    search,
    grid,
  );
  document.body.appendChild(root);

  let onPick: (emoji: string) => void = () => {};
  /** Index into the flattened result list, for keyboard selection. */
  let active = 0;
  let shown: Emoji[] = [];

  const button = (e: Emoji, i: number) =>
    el('button', {
      class: `emoji-cell${i === active ? ' active' : ''}`,
      text: e.char,
      title: `:${e.name}:`,
      type: 'button',
      on: {
        click: () => {
          onPick(e.char);
          close();
        },
      },
    });

  const render = () => {
    const query = search.value.trim();
    if (query) {
      shown = searchEmoji(query, 60);
      replace(
        grid,
        shown.length === 0
          ? [el('div', { class: 'empty', text: 'No emoji match that.' })]
          : shown.map(button),
      );
      return;
    }
    // No query: keep the category headings, which are what make a long grid
    // browsable rather than a wall.
    shown = allEmoji();
    const nodes: HTMLElement[] = [];
    let i = 0;
    for (const category of categories()) {
      nodes.push(el('div', { class: 'emoji-category', text: category }));
      const row = el('div', { class: 'emoji-row' });
      for (const e of shown) {
        if (e.category === category) row.appendChild(button(e, i++));
      }
      nodes.push(row);
    }
    replace(grid, nodes);
  };

  const close = () => {
    root.hidden = true;
    search.value = '';
    active = 0;
  };

  search.addEventListener('input', () => {
    active = 0;
    render();
  });

  search.addEventListener('keydown', (ev: KeyboardEvent) => {
    if (ev.key === 'Escape') {
      ev.preventDefault();
      close();
      return;
    }
    if (ev.key === 'Enter') {
      ev.preventDefault();
      const picked = shown[active];
      if (picked) {
        onPick(picked.char);
        close();
      }
      return;
    }
    if (ev.key === 'ArrowRight' || ev.key === 'ArrowLeft') {
      ev.preventDefault();
      const n = shown.length;
      if (n === 0) return;
      active = (active + (ev.key === 'ArrowRight' ? 1 : -1) + n) % n;
      render();
    }
  });

  // Dismiss on an outside click. Captured on the document rather than via a
  // backdrop element, so the picker never covers the page it floats over.
  document.addEventListener('mousedown', (ev) => {
    if (root.hidden) return;
    const target = ev.target as Node;
    if (!root.contains(target)) close();
  });

  const open = (opts: OpenOptions) => {
    onPick = opts.onPick;
    active = 0;
    root.hidden = false;
    render();
    position(root, opts.anchor);
    search.focus();
  };

  return { root, open, close };
}

/**
 * Place the popover near its anchor, flipping when it would leave the viewport.
 *
 * Fixed positioning against the anchor's viewport rect, so the picker does not
 * inherit the clipping of whatever scroll container the button lives in — a
 * message hover bar is inside an `overflow: hidden` virtual list.
 */
function position(root: HTMLElement, anchor: HTMLElement): void {
  const rect = anchor.getBoundingClientRect();
  // Measure after unhiding; the element has no box while `hidden`.
  const { width, height } = root.getBoundingClientRect();
  const margin = 8;

  let left = rect.left;
  if (left + width > window.innerWidth - margin) left = window.innerWidth - width - margin;
  if (left < margin) left = margin;

  // Prefer above the anchor, which is where a composer button wants it; drop
  // below when there is no room up there.
  let top = rect.top - height - margin;
  if (top < margin) top = Math.min(rect.bottom + margin, window.innerHeight - height - margin);

  root.style.left = `${Math.round(left)}px`;
  root.style.top = `${Math.round(Math.max(margin, top))}px`;
}
