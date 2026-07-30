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

// The service worker backs the offline shell and receives push notifications.
// Registered after mount is scheduled, and failures are swallowed: it is an
// enhancement, and an unsupported browser or an insecure origin must not stop
// the app from starting.
if ('serviceWorker' in navigator) {
  addEventListener('load', () => {
    void navigator.serviceWorker.register('/sw.js').catch((err: unknown) => {
      console.warn('service worker registration failed', err);
    });
  });
}

try {
  mount(root);
} catch (err) {
  // A crash during boot leaves a blank page, which is the least debuggable
  // possible outcome. Say something.
  console.error('failed to start', err);
  root.textContent = 'TensorChat failed to start. Check the console for details.';
}
