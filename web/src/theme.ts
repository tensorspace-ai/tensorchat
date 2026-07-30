/**
 * Light / dark theme selection.
 *
 * Three states, not two: **system**, **light**, **dark**. "System" is the
 * default and is not the same as "light" — it tracks the OS preference as it
 * changes, which is the whole point for anyone whose machine switches at dusk.
 *
 * ## Why an attribute rather than a media query
 *
 * The stylesheet's light palette is keyed on `:root[data-theme='light']` and
 * appears exactly once. The alternative — a `prefers-color-scheme` block *plus*
 * an override block — would mean two copies of forty-odd colour values that
 * could drift apart, in a file whose header rule is that colours live in one
 * place. So the resolved theme is always stamped onto `<html>`, by
 * `theme-boot.ts` before first paint and by [`applyTheme`] afterwards, and CSS
 * only ever reads the attribute.
 */

export type ThemePreference = 'system' | 'light' | 'dark';

const KEY = 'tc_theme';

/** Media query for the OS preference. Held once; it is also the change source. */
function systemQuery(): MediaQueryList | null {
  return typeof matchMedia === 'function' ? matchMedia('(prefers-color-scheme: light)') : null;
}

export function readPreference(): ThemePreference {
  try {
    const v = localStorage.getItem(KEY);
    if (v === 'light' || v === 'dark' || v === 'system') return v;
  } catch {
    // Private browsing. The default is right for this session.
  }
  return 'system';
}

/** Turn a preference into the theme actually in force right now. */
export function resolveTheme(pref: ThemePreference): 'light' | 'dark' {
  if (pref !== 'system') return pref;
  return systemQuery()?.matches ? 'light' : 'dark';
}

/**
 * Stamp the resolved theme on the document.
 *
 * Also sets `color-scheme`, which is what makes the browser's own furniture —
 * scrollbars, form controls, the autofill highlight — match. Without it a light
 * page keeps dark native scrollbars.
 */
export function applyTheme(pref: ThemePreference): void {
  const resolved = resolveTheme(pref);
  const root = document.documentElement;
  root.dataset.theme = resolved;
  root.style.colorScheme = resolved;
}

export function setPreference(pref: ThemePreference): void {
  try {
    localStorage.setItem(KEY, pref);
  } catch {
    // Unwritable storage still gets the theme for this session.
  }
  applyTheme(pref);
}

/**
 * Keep "system" honest: re-resolve when the OS flips.
 *
 * Only meaningful while the preference *is* "system" — an explicit choice
 * deliberately ignores the OS — so the listener re-reads the preference on each
 * change rather than being torn down and rebuilt when it changes.
 */
export function watchSystemTheme(): void {
  const q = systemQuery();
  q?.addEventListener('change', () => {
    if (readPreference() === 'system') applyTheme('system');
  });
}
