/**
 * Theme handling: follow `prefers-color-scheme` unless the user picked a theme
 * explicitly, in which case the choice is persisted in localStorage.
 */

const STORAGE_KEY = 'ferroma.theme';
const MODES = ['auto', 'light', 'dark'];

const listeners = new Set();
let mode = 'auto';

function safeRead() {
  try {
    return window.localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

function safeWrite(value) {
  try {
    window.localStorage.setItem(STORAGE_KEY, value);
  } catch {
    /* storage unavailable (private mode); the in-memory mode still applies */
  }
}

/** Apply the current mode to <html>. */
function apply() {
  const root = document.documentElement;
  root.setAttribute('data-theme', mode);
  const dark =
    mode === 'dark' ||
    (mode === 'auto' && window.matchMedia('(prefers-color-scheme: dark)').matches);
  root.style.colorScheme = dark ? 'dark' : 'light';
}

/** Read the persisted mode without touching the DOM. */
export function currentTheme() {
  return mode;
}

/**
 * @param {string} next one of `auto`, `light`, `dark`
 */
export function setTheme(next) {
  mode = MODES.includes(next) ? next : 'auto';
  safeWrite(mode);
  apply();
  for (const listener of listeners) listener(mode);
}

/** @param {(mode: string) => void} listener */
export function onThemeChange(listener) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Called once at boot, before the first paint of the app. */
export function initTheme() {
  const stored = safeRead();
  mode = MODES.includes(stored) ? stored : 'auto';
  apply();
  const media = window.matchMedia('(prefers-color-scheme: dark)');
  const onChange = () => {
    if (mode === 'auto') apply();
  };
  if (typeof media.addEventListener === 'function') media.addEventListener('change', onChange);
  else if (typeof media.addListener === 'function') media.addListener(onChange);
}

/** The theme list the settings dialog offers. */
export const THEME_MODES = MODES.slice();
