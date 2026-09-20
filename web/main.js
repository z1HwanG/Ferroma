/**
 * Ferroma Webmail shell.
 *
 * Owns boot, authentication, routing, the keyboard map and the bulk-action bar.
 * The feature modules own their own panes and only ever touch the shared store.
 *
 * Keyboard map
 *   c          compose
 *   /          focus search
 *   j          focus the message list
 *   ↑ / ↓      move through the message list
 *   Enter      open the focused message
 *   Space      select the focused message
 *   Esc        close the account menu, leave the reader
 */

import { API_BASE, ApiError, clearTokens, onConnectionChange, query, request, setUnauthorizedHandler } from '../shared/api.js';
import { mailboxesOf } from '../shared/data.js';
import { byId, el, setHidden, setText } from '../shared/dom.js';
import { folderLabel, initFolders, loadFolders, renderFolders, renderMailboxes } from './folders.js';
import { LOCALES, currentLocale, setLocale, t, tn } from '../shared/i18n.js';
import {
  applySeen,
  clearChecked,
  focusList,
  initList,
  loadMessages,
  renderList,
  removeFromList,
  toggleStar,
} from './list.js';
import { initLogin, setSignedInHandler } from './login.js';
import { isOpen as composeIsOpen, openCompose, setSentHandler } from './compose.js';
import { confirmDialog, openModal } from '../shared/modal.js';
import { closeReader, initReader, openMessage, renderChrome, renderReader } from './reader.js';
import { folderBySlug, folderSlug, goFolder, goSearch, onRouteChange, parseHash } from './router.js';
import { openSettings } from './settings.js';
import { getPrefs, getState, mutate, subscribe } from './store.js';
import { initTheme, onThemeChange, toggleTheme } from '../shared/theme.js';
import { toastError, toastInfo, toastSuccess } from '../shared/toast.js';

const MOVE_DIALOG_ID = 'bulk-move-select';

/* --------------------------------------------------------------------- boot */

initTheme();

let showLogin = () => {};
/** @type {Promise<void>|null} */
let booting = null;

/**
 * Tell `shared/diag.js` that a view is on screen, so its watchdog stands down.
 *
 * The guard cannot live here: it has to survive this module failing to load, which is why
 * it is a separate script with no imports. The sign-in panel counts — a page that reached
 * it is not blank.
 */
function viewShown() {
  if (typeof window.__ferromaReady === 'function') window.__ferromaReady();
}

/**
 * Fill the sign-in card's language picker.
 *
 * It is built here rather than in the HTML for the same reason the Settings one is: the
 * shipped locales are a list, not a fixed pair of buttons. Picking one applies at once and
 * reloads — the sign-in card is not a form with a Save button, and every string on it has
 * to be redrawn in the language that was just chosen.
 */
function initLoginLanguage() {
  const host = byId('login-language');
  for (const entry of LOCALES) {
    const button = el('button', {
      type: 'button',
      class: 'segmented-option',
      role: 'radio',
      'aria-checked': entry.tag === currentLocale() ? 'true' : 'false',
      dataset: { tag: entry.tag },
      text: entry.label,
    });
    button.addEventListener('click', () => {
      if (entry.tag === currentLocale()) return;
      setLocale(entry.tag);
      window.location.reload();
    });
    host.append(button);
  }
}

function start() {
  // `shared/diag.js` guards against a blank page; a view on screen — the sign-in panel
  // included — is what tells it to stand down.
  const panel = initLogin();
  initLoginLanguage();
  showLogin = () => {
    panel();
    viewShown();
  };
  // Signing in has to apply the route as well as load the account: the shell's own
  // `boot()` at page load ran while there was no session, so its route application was
  // spent on an empty folder list. Without this the message list stayed empty until the
  // user clicked a *different* folder — the folder in the URL was already the current
  // route, so re-selecting it changed no hash and triggered no load.
  setSignedInHandler(async () => {
    await boot();
    applyRoute(parseHash());
  });

  initFolders({
    onSelectFolder: (folder) => {
      const state = getState();
      // Clicking the folder that is already open re-reads it. `goFolder` cannot express
      // that — the hash does not change, so no route event fires — and a refresh you can
      // ask for is the whole point of clicking a folder twice.
      if (state.folder && state.folder.id === folder.id) {
        refreshFolder(folder);
        return;
      }
      goFolder(folderSlug(folder));
    },
    onSelectMailbox: (mailboxId) => {
      selectMailbox(mailboxId);
    },
  });

  initList({
    onOpen: (id) => openMessageById(id),
    onToggleStar: (message) => toggleStar(message),
  });

  initReader({
    onCompose: (mode, message) => {
      openCompose({ mode, message });
    },
    onAfterChange: () => {
      renderList();
      refreshFoldersQuietly();
    },
    onBack: () => {
      closeReader();
      renderChrome();
      goFolder(getState().folderSlug);
    },
  });

  setSentHandler(() => {
    toastInfo(t('Sent messages appear in the Sent folder.'));
    refreshFoldersQuietly();
  });

  subscribe(onStateChange);
  renderChrome();

  wireChrome();
  wireKeyboard();
  wireBulkActions();

  onRouteChange((route) => {
    applyRoute(route);
  });

  setUnauthorizedHandler(() => {
    clearSession();
    showLogin();
    toastError(t('Your session ended. Sign in again.'));
  });

  onConnectionChange((online) => {
    setHidden(byId('offline-banner'), online);
    mutate((state) => {
      state.offline = !online;
    });
  });

  boot().then(() => {
    applyRoute(parseHash());
  });
}

/** Load the account and the first page of folders/messages. */
function boot() {
  if (booting) return booting;
  booting = (async () => {
    try {
      const account = await request(`${API_BASE}/auth/me`, { toast: false, retryOn401: false });
      const mailboxes = mailboxesOf(account && account.mailboxes ? account.mailboxes : []);
      mutate((state) => {
        state.account = account || null;
        state.user = account || null;
        state.mailboxes = mailboxes;
        if (!state.mailboxId) {
          const primary = mailboxes.find((mailbox) => mailbox.isPrimary) || mailboxes[0];
          state.mailboxId = primary ? primary.id : 0;
        }
      });
      setHidden(byId('login-view'), true);
      setHidden(byId('app-view'), false);
      viewShown();
      renderMailboxes();
      renderAccountMenu();
      await selectMailbox(getState().mailboxId, { silent: true });
    } catch (error) {
      // A server with no administrator has nobody to sign in as, and the Webmail cannot
      // create an account for itself. The first-run wizard lives in the Admin console and
      // needs no session, so a fresh install is sent there rather than left staring at a
      // sign-in panel whose credentials cannot exist yet.
      const setup = await fetchSetupStatus();
      if (setup && setup.required) {
        window.location.replace('admin/#/setup');
        return;
      }
      if (error instanceof ApiError && error.network) {
        setHidden(byId('offline-banner'), false);
        toastError(t('Ferroma could not be reached. Showing the sign-in panel.'));
      }
      showLogin();
    } finally {
      booting = null;
    }
  })();
  return booting;
}

/** Whether the server still needs its first administrator. */
async function fetchSetupStatus() {
  try {
    return await request(`${API_BASE}/setup`, { toast: false, retryOn401: false });
  } catch {
    return null;
  }
}

function clearSession() {
  clearTokens();
  mutate((state) => {
    state.account = null;
    state.user = null;
    state.mailboxes = [];
    state.mailboxId = 0;
    state.folders = [];
    state.folder = null;
    state.folderSlug = 'inbox';
    state.messages = [];
    state.total = 0;
    state.hasMore = false;
    state.selected = null;
    state.selectedId = 0;
    state.search = '';
    state.checked = new Set();
    state.view = 'list';
  });
}

/**
 * Name what is open in the tab title, so a row of mail tabs is readable.
 *
 * `i18n.js` translates `<title>` at load, so the base is already `Ferroma 网页邮箱` on a
 * Chinese interface; this only prefixes the folder or the search. It was the one piece
 * of navigation that never followed the shell.
 *
 * @param {string} label
 */
function setDocumentTitle(label) {
  document.title = label ? `${label} · ${BASE_TITLE}` : BASE_TITLE;
}

/* ------------------------------------------------------------------- routing */

/** Key of the route currently on screen, so a repaint is not a reload. */
let currentRoute = null;
/** The application name as the document already spells it, for the tab title. */
const BASE_TITLE = document.title;

function applyRoute(route) {
  const state = getState();
  const key = `${route.name}|${route.slug}|${route.search}`;
  const sameRoute = key === currentRoute;
  currentRoute = key;

  if (route.name === 'search') {
    const wanted = route.search;
    setText(byId('search-input'), wanted);
    setText(byId('list-title'), t('Search: {query}', { query: wanted }));
    setDocumentTitle(t('Search: {query}', { query: wanted }));
    if (!sameRoute || state.search !== wanted) {
      mutate((draft) => {
        draft.search = wanted;
        draft.messages = [];
        draft.total = 0;
        draft.checked = new Set();
      });
      setView(route.messageId ? 'reader' : 'list');
      loadMessages({ reset: true });
    }
    if (route.messageId) openMessageById(route.messageId);
    renderChrome();
    return;
  }

  const folder = folderBySlug(state.folders, route.slug);
  if (!folder) {
    renderChrome();
    return;
  }

  const folderChanged = !state.folder || folder.id !== state.folder.id;
  mutate((draft) => {
    draft.folder = folder;
    draft.folderSlug = folderSlug(folder);
    draft.search = '';
    if (folderChanged) {
      draft.messages = [];
      draft.total = 0;
      draft.checked = new Set();
    }
  });
  setText(byId('list-title'), folderLabel(folder));
  setDocumentTitle(folderLabel(folder));
  setText(byId('search-input'), '');

  // Reloading on every repaint would fight the reading pane, so the list is fetched
  // when the folder changes and when there is nothing on screen for it yet. The second
  // half matters after signing in: the boot before the session existed already ran this
  // function for the same route, so `sameRoute` is true even though no page was ever
  // loaded — the folder the URL names would otherwise stay empty until another folder
  // was clicked.
  if (folderChanged || state.messages.length === 0) loadMessages({ reset: true });

  setView(route.messageId ? 'reader' : 'list');
  if (route.messageId) {
    openMessageById(route.messageId);
  } else if (state.selected) {
    mutate((draft) => {
      draft.selected = null;
      draft.selectedId = 0;
    });
  }
  renderChrome();
}

function setView(view) {
  mutate((draft) => {
    draft.view = view;
  });
  byId('app-view').dataset.view = view;
}

/** Open a message, whether or not it is in the loaded page. */
function openMessageById(id) {
  const state = getState();
  if (!id) return;
  if (state.selected && state.selected.id === id) return;
  const messageId = Number(id);
  // A draft is not something to read, it is something to finish. Opening it in the
  // reading pane showed the half-written message with no way to continue it, which is
  // what "the Drafts folder cannot be edited" looked like from the outside.
  const listed = state.messages.find((message) => message.id === messageId);
  if (listed && listed.draft) {
    openDraft(messageId);
    return;
  }
  if (state.view !== 'reader') setView('reader');
  // `replace`, not `push`: the click already lands here, so a pushed entry would
  // make Back step through two states of the same action.
  goFolder(state.folderSlug, messageId, { replace: true });
  openMessage(messageId);
}

/**
 * Keep the toggle's pressed state in step with the theme actually in force.
 *
 * The icon is chosen by CSS from `html[data-theme-resolved]`, so a mode of `auto` shows
 * whichever glyph matches the reader's system; `aria-pressed` has to agree with that
 * glyph, which is why it is derived from the resolved scheme rather than from the mode.
 */
function syncThemeButton() {
  const dark = document.documentElement.getAttribute('data-theme-resolved') === 'dark';
  // The sign-in card carries its own toggle: it is the one screen a reader sees before
  // there is a top bar at all, and it keeps the same pressed state.
  for (const id of ['theme-toggle', 'login-theme-toggle']) {
    const button = document.getElementById(id);
    if (button) button.setAttribute('aria-pressed', dark ? 'true' : 'false');
  }
}

/**
 * Continue the stored draft behind a Drafts-folder message.
 *
 * The list gives the *message* id; the draft record is what carries the editable fields,
 * so the two are matched by the `message_id` the server records when a draft is mirrored
 * into the folder.
 */
async function openDraft(messageId) {
  try {
    const page = await request(`${API_BASE}/drafts${query({ limit: 200 })}`, { toast: false });
    const items = page && Array.isArray(page.items) ? page.items : [];
    const draft = items.find((item) => Number(item.message_id) === Number(messageId));
    if (!draft) {
      toastError(t('This draft could not be found. It may have been sent already.'));
      return;
    }
    const addresses = (list) =>
      (Array.isArray(list) ? list : []).map((entry) => (entry && entry.address ? entry.address : '')).filter(Boolean);
    openCompose({
      mode: 'draft',
      draftId: Number(draft.id),
      to: addresses(draft.to),
      cc: addresses(draft.cc),
      bcc: addresses(draft.bcc),
      subject: draft.subject || '',
      text: draft.text || '',
      html: draft.html || undefined,
    });
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : t('This draft could not be opened.'));
  }
}

/* -------------------------------------------------------------------- chrome */

function wireChrome() {
  byId('search-form').addEventListener('submit', (event) => {
    event.preventDefault();
    const value = byId('search-input').value.trim();
    const state = getState();
    if (value === '') {
      goFolder(state.folderSlug);
      return;
    }
    goSearch(value);
  });

  byId('compose-button').addEventListener('click', () => {
    openCompose({ mode: 'new' });
  });

  byId('account-settings').addEventListener('click', () => {
    closeAccountMenu();
    openSettings();
  });

  byId('account-button').addEventListener('click', (event) => {
    event.stopPropagation();
    const menu = byId('account-menu');
    const open = menu.hidden;
    setHidden(menu, !open);
    byId('account-button').setAttribute('aria-expanded', open ? 'true' : 'false');
    if (open) {
      const first = menu.querySelector('button');
      if (first instanceof HTMLElement) first.focus();
    }
  });

  byId('account-logout').addEventListener('click', async () => {
    closeAccountMenu();
    try {
      await request(`${API_BASE}/auth/logout`, { method: 'POST', toast: false, retryOn401: false });
    } catch (error) {
      if (!(error instanceof ApiError)) throw error;
    }
    clearSession();
    showLogin();
    toastSuccess(t('Signed out.'));
  });

  document.addEventListener('click', (event) => {
    const menu = byId('account-menu');
    if (menu.hidden) return;
    const target = event.target;
    if (target instanceof Element && target.closest('#account-menu')) return;
    if (target instanceof Element && target.closest('#account-button')) return;
    closeAccountMenu();
  });

  // The clicked button is the centre of the reveal, so both of them pass themselves.
  const flipTheme = (event) => toggleTheme({ origin: event.currentTarget });
  byId('theme-toggle').addEventListener('click', flipTheme);
  byId('login-theme-toggle').addEventListener('click', flipTheme);
  syncThemeButton();
  onThemeChange(syncThemeButton);

  byId('offline-retry').addEventListener('click', () => {
    boot().then(() => {
      loadFolders(getState().mailboxId).then(() => {
        renderFolders();
        loadMessages({ reset: true });
      });
    });
  });
}

function closeAccountMenu() {
  setHidden(byId('account-menu'), true);
  byId('account-button').setAttribute('aria-expanded', 'false');
}

function renderAccountMenu() {
  const state = getState();
  const name = state.user && state.user.display_name ? state.user.display_name : getPrefs().displayName;
  setText(byId('account-menu-name'), name || t('Signed in'));
  setText(byId('account-menu-email'), (state.user && state.user.email) || '');
  // The console is admin-only, so the entry is only offered to an administrator. The
  // server refuses everyone else regardless — this is a shortcut, not the check.
  setHidden(byId('account-admin'), !(state.user && state.user.is_admin));
}

/* ----------------------------------------------------------------- keyboard */

function isTypingTarget(target) {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || target.isContentEditable;
}

function wireKeyboard() {
  document.addEventListener('keydown', (event) => {
    if (event.defaultPrevented || event.ctrlKey || event.metaKey || event.altKey) return;
    const typing = isTypingTarget(event.target);

    if (event.key === 'Escape') {
      const menu = byId('account-menu');
      if (!menu.hidden) {
        closeAccountMenu();
        byId('account-button').focus();
        return;
      }
      if (typing) return;
      if (getState().view === 'reader' && !composeIsOpen()) {
        closeReader();
        renderChrome();
        goFolder(getState().folderSlug);
      }
      return;
    }

    if (typing) return;

    if (event.key === 'c' || event.key === 'C') {
      if (composeIsOpen()) return;
      event.preventDefault();
      // The keyboard map must agree with the disabled Compose button: an account with no
      // address has no `From` to send from.
      if (getState().mailboxes.length === 0) {
        toastError(t('Add an address to this account before composing.'));
        return;
      }
      openCompose({ mode: 'new' });
      return;
    }

    if (event.key === '/') {
      event.preventDefault();
      byId('search-input').focus();
      byId('search-input').select();
      return;
    }

    if (event.key === 'j' && !event.shiftKey) {
      event.preventDefault();
      focusList();
    }
  });
}

/* -------------------------------------------------------------- bulk actions */

function wireBulkActions() {
  byId('bulk-bar').addEventListener('click', async (event) => {
    const target = event.target instanceof Element ? event.target.closest('[data-bulk]') : null;
    if (!target) return;
    const action = target.dataset.bulk;
    const state = getState();
    const ids = Array.from(state.checked);
    if (ids.length === 0) return;

    if (action === 'clear') {
      clearChecked();
      return;
    }

    if (action === 'move') {
      openBulkMoveDialog(ids);
      return;
    }

    if (action === 'delete') {
      await bulkDelete(ids);
      return;
    }

    await runBatch(action, ids);
  });
}

/**
 * `POST /api/v1/messages/batch` with the documented operation names.
 * @param {string} operation
 * @param {number[]} ids
 * @param {number} [folderId]
 */
async function runBatch(operation, ids, folderId) {
  const body = { operation, ids };
  if (folderId) body.folder_id = folderId;
  try {
    await request(`${API_BASE}/messages/batch`, { method: 'POST', body, toast: false });
    if (operation === 'read') applySeen(ids, true);
    if (operation === 'unread') applySeen(ids, false);
    if (operation === 'flag' || operation === 'unflag') {
      const flagged = operation === 'flag';
      mutate((draft) => {
        draft.messages = draft.messages.map((message) =>
          ids.includes(message.id) ? Object.assign({}, message, { flagged }) : message,
        );
      });
      renderList();
    }
    if (operation === 'move' || operation === 'delete') removeFromList(ids);
    clearChecked();
    const labels = {
      read: t('Marked read'),
      unread: t('Marked unread'),
      flag: t('Starred'),
      unflag: t('Stars removed'),
      move: t('Moved'),
      delete: t('Moved to Trash'),
    };
    const label = labels[operation] || t('Updated');
    toastSuccess(
      `${label}: ${tn(ids.length, '{count} message.', '{count} messages.', { count: ids.length })}`,
    );
    refreshFoldersQuietly();
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : t('The action could not be completed.'));
  }
}

async function bulkDelete(ids) {
  const state = getState();
  const trash = state.folders.find((folder) => folder.specialUse === 'trash');
  const inTrash = Boolean(state.folder && trash && state.folder.id === trash.id);
  const confirmed = await confirmDialog({
    title: inTrash ? t('Delete permanently') : t('Move to Trash'),
    message: inTrash
      ? t('{count} messages will be removed for good. This cannot be undone.', { count: ids.length })
      : t('{count} messages will be moved to Trash.', { count: ids.length }),
    confirmLabel: inTrash ? t('Delete permanently') : t('Move to Trash'),
  });
  if (!confirmed) return;

  if (!inTrash) {
    await runBatch('delete', ids);
    return;
  }

  try {
    await Promise.all(
      ids.map((id) => request(`${API_BASE}/messages/${id}?permanent=true`, { method: 'DELETE', toast: false })),
    );
    removeFromList(ids);
    clearChecked();
    toastSuccess(t('{count} messages deleted permanently.', { count: ids.length }));
    refreshFoldersQuietly();
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : t('The messages could not be deleted.'));
  }
}

async function openBulkMoveDialog(ids) {
  const state = getState();
  const others = state.folders.filter((folder) => !state.folder || folder.id !== state.folder.id);
  if (others.length === 0) {
    toastError(t('There is no other folder to move these messages to.'));
    return;
  }
  const select = el('select', { class: 'input', id: MOVE_DIALOG_ID, size: String(Math.min(others.length, 8)) });
  for (const folder of others) {
    select.append(el('option', { value: String(folder.id), text: folder.name }));
  }
  const body = el('div', {}, [
    el('p', {
      class: 'modal-message',
      text: t('Move {count} selected messages to:', { count: ids.length }),
    }),
    el('label', { class: 'visually-hidden', for: MOVE_DIALOG_ID, text: t('Destination folder') }),
    select,
  ]);
  const cancel = el('button', { type: 'button', class: 'btn', text: t('Cancel') });
  const move = el('button', { type: 'button', class: 'btn btn-primary', text: t('Move') });
  const modal = openModal({
    title: t('Move messages'),
    body,
    footer: [cancel, move],
    onMount: () => {
      cancel.addEventListener('click', () => modal.close('cancel'));
      move.addEventListener('click', () => {
        const folder = others.find((candidate) => candidate.id === Number.parseInt(select.value, 10));
        modal.close('move');
        if (folder) runBatch('move', ids, folder.id);
      });
    },
  });
}

/* -------------------------------------------------------------------- actions */

/** Switch send-from address and reload its folders and messages. */
async function selectMailbox(mailboxId, options = {}) {
  const state = getState();
  let target = mailboxId;
  if (!target) {
    const primary = state.mailboxes.find((mailbox) => mailbox.isPrimary) || state.mailboxes[0];
    target = primary ? primary.id : 0;
  }
  if (!target) {
    renderMailboxes();
    return;
  }

  mutate((draft) => {
    draft.mailboxId = target;
    draft.folders = [];
    draft.messages = [];
    draft.total = 0;
    draft.selected = null;
    draft.selectedId = 0;
    draft.checked = new Set();
    draft.folder = null;
  });
  renderMailboxes();

  await loadFolders(target);
  const folders = getState().folders;
  const wanted = folderBySlug(folders, parseHash().slug);
  mutate((draft) => {
    draft.folder = wanted;
    draft.folderSlug = folderSlug(wanted);
  });
  renderFolders();
  if (!options.silent) {
    setText(byId('list-title'), wanted ? folderLabel(wanted) : t('Messages'));
    setDocumentTitle(wanted ? folderLabel(wanted) : t('Messages'));
    loadMessages({ reset: true });
  }
}

/**
 * Re-read the folder that is already on screen: its tree entry, then its messages.
 *
 * Deliberately not silent: the reader asked for it, so the list repaints (and the pane's
 * heading keeps the folder's name instead of falling back to the folder tree's first
 * entry).
 */
async function refreshFolder(folder) {
  const state = getState();
  setText(byId('list-title'), folderLabel(folder));
  await loadFolders(state.mailboxId);
  renderFolders();
  await loadMessages({ reset: true });
}

/** Refresh unread counts without disturbing the list. */
async function refreshFoldersQuietly() {
  const state = getState();
  if (!state.mailboxId) return;
  await loadFolders(state.mailboxId);
  renderFolders();
}

/* --------------------------------------------------------------- state glue */

function onStateChange() {
  renderList();
  renderReader();
  renderFolders();
}

/* ------------------------------------------------------------------- start */

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', start, { once: true });
} else {
  start();
}
