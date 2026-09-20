/**
 * Application state for the Webmail shell: one observable object plus the
 * preferences that live in localStorage.
 */

const PREF_KEY = 'ferroma.webmail.prefs';

const DEFAULT_PREFS = {
  displayName: '',
  signature: '',
  theme: 'light',
  perPage: 50,
  markReadOnOpen: true,
};

function readPrefs() {
  try {
    const stored = JSON.parse(window.localStorage.getItem(PREF_KEY) || '{}');
    if (!stored || typeof stored !== 'object') return Object.assign({}, DEFAULT_PREFS);
    return Object.assign({}, DEFAULT_PREFS, stored);
  } catch {
    return Object.assign({}, DEFAULT_PREFS);
  }
}

let prefs = readPrefs();

/**
 * @param {Partial<typeof DEFAULT_PREFS>} patch
 */
export function savePrefs(patch) {
  prefs = Object.assign({}, prefs, patch);
  try {
    window.localStorage.setItem(PREF_KEY, JSON.stringify(prefs));
  } catch {
    /* storage unavailable: preferences stay in memory for this session */
  }
  return prefs;
}

export function getPrefs() {
  return Object.assign({}, prefs);
}

/** @type {typeof DEFAULT_PREFS} */
export const PREF_DEFAULTS = DEFAULT_PREFS;

export const MESSAGES_PER_PAGE_CHOICES = [25, 50, 100, 200];

/* -------------------------------------------------------------------- state */

const state = {
  account: null,
  user: null,
  mailboxes: [],
  mailboxId: 0,
  folders: [],
  folder: null,
  folderSlug: 'inbox',
  messages: [],
  total: 0,
  offset: 0,
  hasMore: false,
  listLoading: false,
  selectedId: 0,
  selected: null,
  readerLoading: false,
  search: '',
  checked: new Set(),
  lastCheckedIndex: -1,
  offline: false,
  view: 'list',
};

const listeners = new Set();

export function getState() {
  return state;
}

/**
 * Mutate state in place (for arrays and sets) and notify once.
 * @param {(state: typeof state) => void} mutate
 */
export function mutate(mutate) {
  mutate(state);
  notify();
}

/** @param {() => void} listener */
export function subscribe(listener) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function notify() {
  for (const listener of Array.from(listeners)) listener(state);
}
