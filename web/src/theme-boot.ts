/**
 * Stamp the theme onto `<html>` before the first paint.
 *
 * Built as a separate, tiny, *classic* script that `index.html` loads in
 * `<head>` without `defer`. That is the only way to run code before the browser
 * paints: the main bundle is a module, and modules are deferred until after the
 * document is parsed, so a light-preference user would get a flash of the dark
 * default on every single load.
 *
 * The usual trick for this is a few lines inlined into the `<head>`. That is not
 * available here — the Content-Security-Policy ships without `unsafe-inline`,
 * deliberately, and weakening it to save one request would be a bad trade for a
 * page whose whole rendering path is hand-built to avoid injection.
 *
 * Kept dependency-free on purpose: this file is duplicated logic-wise with
 * `theme.ts` by necessity, since importing it would pull the module graph in and
 * defeat the point. It is six lines, and `theme.ts` owns everything else.
 */

try {
  const stored = localStorage.getItem('tc_theme');
  const pref = stored === 'light' || stored === 'dark' ? stored : 'system';
  const resolved =
    pref === 'system'
      ? matchMedia('(prefers-color-scheme: light)').matches
        ? 'light'
        : 'dark'
      : pref;
  document.documentElement.dataset.theme = resolved;
  document.documentElement.style.colorScheme = resolved;
} catch {
  // Storage or matchMedia unavailable. The stylesheet's default is dark, which
  // is what an unstamped document already shows.
}
