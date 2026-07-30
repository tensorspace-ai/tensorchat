/**
 * DOM construction helpers.
 *
 * Everything in the UI is built from real nodes — there is no `innerHTML`
 * anywhere in this codebase, and no template string is ever parsed as markup.
 * That is not stylistic: it means user-controlled text cannot become elements,
 * so the app needs no HTML sanitizer and the page can ship a Content-Security-
 * Policy with no `unsafe-inline`.
 */

type Child = Node | string | number | null | undefined | false;

type Props = {
  class?: string;
  text?: string;
  title?: string;
  id?: string;
  type?: string;
  value?: string;
  placeholder?: string;
  href?: string;
  src?: string;
  alt?: string;
  role?: string;
  tabIndex?: number;
  disabled?: boolean;
  hidden?: boolean;
  /** `data-*` attributes. */
  data?: Record<string, string>;
  /** `aria-*` attributes. */
  aria?: Record<string, string>;
  style?: Partial<CSSStyleDeclaration>;
  /** Event handlers, e.g. `on: { click: fn }`. */
  on?: Record<string, EventListenerOrEventListenerObject>;
  [key: string]: unknown;
};

/**
 * Create an element.
 *
 * `props.text` sets `textContent`, which is the safe path and the one used for
 * every piece of server data in the UI.
 */
export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Props = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);

  for (const [key, value] of Object.entries(props)) {
    if (value === undefined || value === null || value === false) continue;
    switch (key) {
      case 'class':
        node.className = value as string;
        break;
      case 'text':
        node.textContent = String(value);
        break;
      case 'data':
        for (const [k, v] of Object.entries(value as Record<string, string>)) {
          node.dataset[k] = v;
        }
        break;
      case 'aria':
        for (const [k, v] of Object.entries(value as Record<string, string>)) {
          node.setAttribute(`aria-${k}`, v);
        }
        break;
      case 'style':
        Object.assign(node.style, value);
        break;
      case 'on':
        for (const [event, handler] of Object.entries(
          value as Record<string, EventListenerOrEventListenerObject>,
        )) {
          node.addEventListener(event, handler);
        }
        break;
      case 'disabled':
      case 'hidden':
      case 'tabIndex':
      case 'value':
        // Properties, not attributes: `value` in particular must be set as a
        // property or the element ignores later programmatic updates.
        (node as unknown as Record<string, unknown>)[key] = value;
        break;
      default:
        node.setAttribute(key, String(value));
    }
  }

  append(node, children);
  return node;
}

export function append(parent: Node, children: Child[]): void {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    parent.appendChild(typeof child === 'object' ? child : document.createTextNode(String(child)));
  }
}

/** Replace all of an element's children in one operation. */
export function replace(parent: Element, children: Child[]): void {
  parent.textContent = '';
  append(parent, children);
}

/** An inline SVG icon from a path definition. */
export function icon(path: string, size = 16): SVGSVGElement {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('width', String(size));
  svg.setAttribute('height', String(size));
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '2');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true');
  const p = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  p.setAttribute('d', path);
  svg.appendChild(p);
  return svg;
}

export const ICONS = {
  hash: 'M4 9h16M4 15h16M10 3L8 21M16 3l-2 18',
  lock: 'M5 11h14v10H5zM8 11V7a4 4 0 018 0v4',
  search: 'M11 19a8 8 0 100-16 8 8 0 000 16zM21 21l-4.35-4.35',
  send: 'M22 2L11 13M22 2l-7 20-4-9-9-4 20-7z',
  smile: 'M12 22a10 10 0 100-20 10 10 0 000 20zM8 14s1.5 2 4 2 4-2 4-2M9 9h.01M15 9h.01',
  thread: 'M21 15a2 2 0 01-2 2H7l-4 4V5a2 2 0 012-2h14a2 2 0 012 2z',
  close: 'M18 6L6 18M6 6l12 12',
  plus: 'M12 5v14M5 12h14',
  paperclip: 'M21.44 11.05l-9.19 9.19a6 6 0 01-8.49-8.49l9.19-9.19a4 4 0 015.66 5.66l-9.2 9.19a2 2 0 01-2.83-2.83l8.49-8.48',
  edit: 'M11 4H4a2 2 0 00-2 2v14a2 2 0 002 2h14a2 2 0 002-2v-7M18.5 2.5a2.12 2.12 0 013 3L12 15l-4 1 1-4 9.5-9.5z',
  trash: 'M3 6h18M8 6V4a2 2 0 012-2h4a2 2 0 012 2v2M19 6l-1 14a2 2 0 01-2 2H8a2 2 0 01-2-2L5 6',
  people: 'M17 21v-2a4 4 0 00-4-4H5a4 4 0 00-4 4v2M9 11a4 4 0 100-8 4 4 0 000 8zM23 21v-2a4 4 0 00-3-3.87M16 3.13a4 4 0 010 7.75',
  pin: 'M12 17v5M9 3h6l-1 6 3 3v2H7v-2l3-3-1-6z',
  bookmark: 'M19 21l-7-5-7 5V5a2 2 0 012-2h10a2 2 0 012 2z',
};

// -- Formatting ----------------------------------------------------------

const TIME_FMT = new Intl.DateTimeFormat(undefined, { hour: 'numeric', minute: '2-digit' });
const DATE_FMT = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' });
const FULL_FMT = new Intl.DateTimeFormat(undefined, {
  weekday: 'long',
  month: 'long',
  day: 'numeric',
  hour: 'numeric',
  minute: '2-digit',
});

export function formatTime(d: Date): string {
  return TIME_FMT.format(d);
}

export function formatFullTime(d: Date): string {
  return FULL_FMT.format(d);
}

/** A day separator label: "Today", "Yesterday", or a date. */
export function formatDayLabel(d: Date): string {
  const today = new Date();
  const startOfToday = new Date(today.getFullYear(), today.getMonth(), today.getDate()).getTime();
  const day = new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const dayMs = 86400000;
  if (day === startOfToday) return 'Today';
  if (day === startOfToday - dayMs) return 'Yesterday';
  return DATE_FMT.format(d);
}

export function sameDay(a: Date, b: Date): boolean {
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB'];
  let v = bytes / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

/**
 * A stable color for an avatar, derived from the user id.
 *
 * Deterministic so a person looks the same across reloads and devices, and
 * derived locally so the server never has to store or serve avatar images.
 */
export function avatarHue(id: string): number {
  let h = 0;
  for (let i = 0; i < id.length; i++) h = (h * 31 + id.charCodeAt(i)) | 0;
  return Math.abs(h) % 360;
}

/** Initials for the avatar placeholder. */
export function initials(name: string): string {
  const parts = name.trim().split(/\s+/).filter(Boolean);
  if (parts.length === 0) return '?';
  if (parts.length === 1) return [...parts[0]][0].toUpperCase();
  return ([...parts[0]][0] + [...parts[parts.length - 1]][0]).toUpperCase();
}
