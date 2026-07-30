/**
 * Entry point. Everything interesting is in `app.ts`; this file exists to
 * find the mount node and to fail visibly rather than silently.
 */

import { mount } from './app.ts';
import { applyTheme, readPreference, watchSystemTheme } from './theme.ts';

// `theme-boot.ts` already stamped the document before first paint. Re-apply
// here so a browser that blocked the classic script still gets the theme, and
// start following the OS for anyone on "system".
applyTheme(readPreference());
watchSystemTheme();

const root = document.getElementById('root');
if (!root) {
  throw new Error('#root is missing from the document');
}

try {
  mount(root);
} catch (err) {
  // A crash during boot leaves a blank page, which is the least debuggable
  // possible outcome. Say something.
  console.error('failed to start', err);
  root.textContent = 'TensorChat failed to start. Check the console for details.';
}
