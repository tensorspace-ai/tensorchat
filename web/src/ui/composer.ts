/**
 * The message composer.
 *
 * Behaviors that matter more than they look:
 *
 * * **Optimistic send.** The message appears immediately, keyed by a nonce the
 *   server echoes back. A round trip should never be visible while typing.
 * * **Throttled typing indicators.** One frame every few seconds, not one per
 *   keystroke — the difference between a handful of frames per message and
 *   dozens.
 * * **Mention autocomplete** over the local user list, so it costs no request.
 * * **Draft persistence** per channel, so switching channels mid-sentence does
 *   not lose the sentence.
 */

import { ICONS, el, icon, replace } from '../dom.ts';
import { effect } from '../signals.ts';
import { expandShortcodes, searchEmoji } from '../emoji.ts';
import type { Attachment, Id, User } from '../protocol.ts';
import type { Store } from '../store.ts';
import { openEmojiPicker } from './emoji-picker.ts';

/** Minimum gap between typing frames sent to the server. */
const TYPING_THROTTLE_MS = 3000;
const MAX_ROWS = 12;

export type ComposerActions = {
  send: (body: string, attachments: Id[]) => void;
  typing: () => void;
  upload: (file: File) => Promise<Attachment>;
  /** Called when the user presses Up on an empty composer. */
  editLast?: () => void;
};

export function Composer(
  store: Store,
  actions: ComposerActions,
  opts: { placeholder?: () => string; threadRoot?: Id } = {},
): HTMLElement {
  const drafts = new Map<Id, string>();
  const staged: Attachment[] = [];
  let lastTypingSent = 0;
  let activeChannel: Id | null = null;

  const input = el('textarea', {
    class: 'composer-input',
    rows: 1,
    placeholder: 'Message',
    'aria-label': 'Message',
  }) as HTMLTextAreaElement;

  const attachmentBar = el('div', { class: 'staged-attachments', hidden: true });
  const suggestions = el('div', { class: 'mention-suggest', hidden: true });

  const fileInput = el('input', { type: 'file', class: 'hidden-file' }) as HTMLInputElement;
  fileInput.multiple = true;

  const sendButton = el(
    'button',
    { class: 'send-button', title: 'Send', disabled: true, on: { click: () => submit() } },
    icon(ICONS.send, 16),
  );

  const root = el(
    'div',
    { class: 'composer' },
    suggestions,
    attachmentBar,
    el(
      'div',
      { class: 'composer-box' },
      el(
        'button',
        { class: 'icon-button', title: 'Attach a file', on: { click: () => fileInput.click() } },
        icon(ICONS.paperclip, 17),
      ),
      input,
      el(
        'button',
        {
          class: 'icon-button',
          title: 'Emoji',
          on: {
            click: (ev: Event) =>
              openEmojiPicker({
                anchor: ev.currentTarget as HTMLElement,
                onPick: insertAtCaret,
              }),
          },
        },
        icon(ICONS.smile, 17),
      ),
      sendButton,
      fileInput,
    ),
    el('div', { class: 'composer-hint', text: 'Enter to send · Shift+Enter for a new line' }),
  );

  // -- Autosizing ---------------------------------------------------------

  function autosize(): void {
    input.style.height = 'auto';
    const lineHeight = 22;
    const max = lineHeight * MAX_ROWS;
    input.style.height = `${Math.min(input.scrollHeight, max)}px`;
    input.style.overflowY = input.scrollHeight > max ? 'auto' : 'hidden';
  }

  // -- Drafts -------------------------------------------------------------

  effect(() => {
    const channel = opts.threadRoot ?? store.currentChannel();
    if (channel === activeChannel) return;
    // Save the outgoing channel's draft before swapping.
    if (activeChannel) drafts.set(activeChannel, input.value);
    activeChannel = channel;
    input.value = channel ? (drafts.get(channel) ?? '') : '';
    autosize();
    updateSendState();
  });

  effect(() => {
    input.placeholder = opts.placeholder?.() ?? 'Message';
  });

  // -- Sending ------------------------------------------------------------

  function updateSendState(): void {
    sendButton.disabled = input.value.trim().length === 0 && staged.length === 0;
  }

  /** Drop text in at the caret and keep the caret after it. */
  function insertAtCaret(text: string): void {
    const start = input.selectionStart ?? input.value.length;
    const end = input.selectionEnd ?? start;
    input.value = input.value.slice(0, start) + text + input.value.slice(end);
    const pos = start + text.length;
    input.setSelectionRange(pos, pos);
    autosize();
    updateSendState();
    input.focus();
  }

  function submit(): void {
    // Expand `:shortcode:` on send rather than on render, so what is stored is
    // the emoji itself and no receiver needs a shortcode table to read it.
    const body = expandShortcodes(input.value.trim());
    if (!body && staged.length === 0) return;
    actions.send(body, staged.map((a) => a.id));

    input.value = '';
    staged.length = 0;
    if (activeChannel) drafts.delete(activeChannel);
    renderStaged();
    autosize();
    updateSendState();
    input.focus();
  }

  input.addEventListener('input', () => {
    autosize();
    updateSendState();
    updateSuggestions();

    // Throttle: one frame every few seconds regardless of typing speed.
    const now = Date.now();
    if (input.value && now - lastTypingSent > TYPING_THROTTLE_MS) {
      lastTypingSent = now;
      actions.typing();
    }
  });

  input.addEventListener('keydown', (ev: KeyboardEvent) => {
    if (suggestionState.open) {
      if (ev.key === 'ArrowDown' || ev.key === 'ArrowUp') {
        ev.preventDefault();
        moveSuggestion(ev.key === 'ArrowDown' ? 1 : -1);
        return;
      }
      if (ev.key === 'Enter' || ev.key === 'Tab') {
        ev.preventDefault();
        acceptSuggestion();
        return;
      }
      if (ev.key === 'Escape') {
        ev.preventDefault();
        closeSuggestions();
        return;
      }
    }

    // Enter sends; Shift+Enter (and IME composition) inserts a newline.
    if (ev.key === 'Enter' && !ev.shiftKey && !ev.isComposing) {
      ev.preventDefault();
      submit();
      return;
    }
    if (ev.key === 'ArrowUp' && input.value === '' && actions.editLast) {
      ev.preventDefault();
      actions.editLast();
    }
  });

  // -- Attachments --------------------------------------------------------

  async function stageFiles(files: FileList | File[]): Promise<void> {
    for (const file of Array.from(files)) {
      const placeholder = el(
        'div',
        { class: 'staged uploading' },
        el('span', { text: file.name }),
        el('span', { class: 'staged-progress', text: 'Uploading…' }),
      );
      attachmentBar.hidden = false;
      attachmentBar.appendChild(placeholder);
      try {
        const a = await actions.upload(file);
        staged.push(a);
        renderStaged();
      } catch (err) {
        placeholder.classList.remove('uploading');
        placeholder.classList.add('failed');
        replace(placeholder, [
          el('span', { text: file.name }),
          el('span', {
            class: 'staged-progress',
            text: err instanceof Error ? err.message : 'Upload failed',
          }),
        ]);
      }
      updateSendState();
    }
  }

  function renderStaged(): void {
    attachmentBar.hidden = staged.length === 0;
    replace(
      attachmentBar,
      staged.map((a) =>
        el(
          'div',
          { class: 'staged' },
          el('span', { class: 'staged-name', text: a.n }),
          el('button', {
            class: 'staged-remove',
            text: '×',
            title: 'Remove',
            on: {
              click: () => {
                const at = staged.indexOf(a);
                if (at !== -1) staged.splice(at, 1);
                renderStaged();
                updateSendState();
              },
            },
          }),
        ),
      ),
    );
  }

  fileInput.addEventListener('change', () => {
    if (fileInput.files) void stageFiles(fileInput.files);
    fileInput.value = '';
  });

  root.addEventListener('dragover', (ev) => {
    ev.preventDefault();
    root.classList.add('drag-over');
  });
  root.addEventListener('dragleave', () => root.classList.remove('drag-over'));
  root.addEventListener('drop', (ev: DragEvent) => {
    ev.preventDefault();
    root.classList.remove('drag-over');
    if (ev.dataTransfer?.files.length) void stageFiles(ev.dataTransfer.files);
  });

  // Pasting an image from the clipboard uploads it, like every other chat app.
  input.addEventListener('paste', (ev: ClipboardEvent) => {
    const files = Array.from(ev.clipboardData?.files ?? []);
    if (files.length) {
      ev.preventDefault();
      void stageFiles(files);
    }
  });

  // -- Autocomplete -------------------------------------------------------
  //
  // `@handle` and `:shortcode:` share one popover, one selection index, and one
  // set of key bindings. They differ only in what they match and what they
  // insert, so they are two shapes of `Suggestion` rather than two mechanisms.

  /** `label` is the primary text, `hint` the dimmed one; `insert` is literal. */
  type Suggestion = { label: string; hint: string; insert: string };

  const suggestionState = { open: false, index: 0, matches: [] as Suggestion[], start: 0 };

  function updateSuggestions(): void {
    const upToCaret = input.value.slice(0, input.selectionStart ?? 0);

    const mention = /@([a-z0-9._-]*)$/i.exec(upToCaret);
    if (mention) {
      const query = mention[1].toLowerCase();
      openSuggestions(
        [...store.users().values()]
          .filter((u: User) => !u.d && (u.h.startsWith(query) || u.n.toLowerCase().includes(query)))
          .slice(0, 8)
          .map((u: User) => ({ label: u.n || u.h, hint: `@${u.h}`, insert: `@${u.h} ` })),
        upToCaret.length - mention[0].length,
      );
      return;
    }

    // Two characters before offering emoji: a bare `:` is far more often
    // punctuation ("see below:") than the start of a shortcode.
    const shortcode = /:([a-z0-9_+-]{2,})$/i.exec(upToCaret);
    if (shortcode) {
      openSuggestions(
        searchEmoji(shortcode[1], 8).map((e) => ({
          label: `${e.char}  :${e.name}:`,
          hint: '',
          insert: `${e.char} `,
        })),
        upToCaret.length - shortcode[0].length,
      );
      return;
    }

    closeSuggestions();
  }

  function openSuggestions(matches: Suggestion[], start: number): void {
    if (matches.length === 0) {
      closeSuggestions();
      return;
    }
    suggestionState.open = true;
    suggestionState.matches = matches;
    suggestionState.index = 0;
    suggestionState.start = start;
    renderSuggestions();
  }

  function renderSuggestions(): void {
    suggestions.hidden = false;
    replace(
      suggestions,
      suggestionState.matches.map((s, i) =>
        el(
          'button',
          {
            class: `suggest-row${i === suggestionState.index ? ' active' : ''}`,
            on: {
              click: () => {
                suggestionState.index = i;
                acceptSuggestion();
              },
            },
          },
          el('span', { class: 'suggest-name', text: s.label }),
          s.hint ? el('span', { class: 'suggest-handle', text: s.hint }) : null,
        ),
      ),
    );
  }

  function moveSuggestion(delta: number): void {
    const n = suggestionState.matches.length;
    suggestionState.index = (suggestionState.index + delta + n) % n;
    renderSuggestions();
  }

  function acceptSuggestion(): void {
    const choice = suggestionState.matches[suggestionState.index];
    if (!choice) return;
    const caret = input.selectionStart ?? input.value.length;
    const before = input.value.slice(0, suggestionState.start);
    const after = input.value.slice(caret);
    input.value = before + choice.insert + after;
    const pos = before.length + choice.insert.length;
    input.setSelectionRange(pos, pos);
    closeSuggestions();
    autosize();
    updateSendState();
    input.focus();
  }

  function closeSuggestions(): void {
    suggestionState.open = false;
    suggestions.hidden = true;
  }

  // Let the app focus the composer from a keyboard shortcut.
  (root as HTMLElement & { focusInput?: () => void }).focusInput = () => input.focus();
  (root as HTMLElement & { setText?: (t: string) => void }).setText = (t: string) => {
    input.value = t;
    autosize();
    updateSendState();
    input.focus();
  };

  return root;
}
