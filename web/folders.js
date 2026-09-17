/**
 * Folder pane: the send-from address picker and the folder tree with unread
 * counts. Special-use folders are identified by `special_use`, never by name, so
 * a localised or renamed "Sent" folder still gets the right icon.
 */

import { API_BASE, ApiError, request } from './api.js';
import { foldersOf } from './data.js';
import { byId, clear, el, labelWithTitle, setHidden, svgIcon } from './dom.js';
import { confirmDialog, promptDialog } from './modal.js';
import { folderSlug } from './router.js';
import { getState, mutate } from './store.js';
import { toastError, toastSuccess } from './toast.js';

const ICON_FOR_SPECIAL = {
  inbox: 'inbox',
  sent: 'sent',
  drafts: 'drafts',
  trash: 'trash',
  junk: 'junk',
  archive: 'archive',
};

/** @type {{onSelectFolder: (folder: object) => void, onSelectMailbox: (mailboxId: number) => void}|null} */
let handlers = null;

/**
 * @param {{onSelectFolder: (folder: object) => void, onSelectMailbox: (mailboxId: number) => void}} options
 */
export function initFolders(options) {
  handlers = options;
  const select = byId('mailbox-select');
  const newFolder = byId('new-folder-button');
  const renameFolder = byId('rename-folder-button');
  const deleteFolder = byId('delete-folder-button');

  select.addEventListener('change', () => {
    const id = Number.parseInt(select.value, 10);
    if (handlers) handlers.onSelectMailbox(Number.isFinite(id) ? id : 0);
  });

  newFolder.addEventListener('click', () => {
    const state = getState();
    if (!state.mailboxId) {
      toastError('Choose a send-from address first.');
      return;
    }
    createFolder(state.mailboxId);
  });

  // Rename and delete act on the folder that is on screen, which is the only one the
  // operator has unambiguously pointed at. Both buttons keep their focus targets in
  // the tree rather than opening a per-row menu no keyboard user could reach.
  renameFolder.addEventListener('click', () => {
    const folder = getState().folder;
    if (!folder) {
      toastError('Open a folder first.');
      return;
    }
    renameFolderTo(folder);
  });

  deleteFolder.addEventListener('click', () => {
    const folder = getState().folder;
    if (!folder) {
      toastError('Open a folder first.');
      return;
    }
    deleteFolderFrom(folder);
  });
}

/** Repaint the address picker from state. */
export function renderMailboxes() {
  const select = byId('mailbox-select');
  const state = getState();
  clear(select);
  for (const mailbox of state.mailboxes) {
    select.append(
      el('option', {
        value: String(mailbox.id),
        text: mailbox.isPrimary ? `${mailbox.address} (primary)` : mailbox.address,
      }),
    );
  }
  select.value = String(state.mailboxId);
  select.disabled = state.mailboxes.length < 2;
  select.title = state.mailboxes.map((mailbox) => mailbox.address).join('\n');
}

/** Repaint the folder tree from state. */
export function renderFolders() {
  const list = byId('folder-list');
  const state = getState();

  const previouslyFocused = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const focusedSlug = previouslyFocused && previouslyFocused.dataset ? previouslyFocused.dataset.folderSlug : null;

  clear(list);

  if (state.folders.length === 0) {
    list.append(el('li', { class: 'empty', text: 'No folders yet.' }));
    return;
  }

  const sorted = state.folders.slice().sort((a, b) => {
    if (a.sortKey !== b.sortKey) return a.sortKey - b.sortKey;
    return a.name.localeCompare(b.name);
  });

  for (const folder of sorted) {
    const active = Boolean(state.folder) && state.folder.id === folder.id;
    const iconName = folder.specialUse ? ICON_FOR_SPECIAL[folder.specialUse] || 'folder' : 'folder';
    const unread = folder.unseenCount;

    const name = el('span', { class: 'folder-name' });
    labelWithTitle(name, folder.name, active ? 'Current folder' : 'Folder');

    const button = el(
      'button',
      {
        type: 'button',
        class: 'btn folder-button',
        'aria-current': active ? 'true' : 'false',
        'aria-label':
          `${folder.name}, ${folder.messageCount} messages, ${unread} unread` +
          (active ? ', current folder' : ''),
        title: `${folder.name} — ${folder.messageCount} messages, ${unread} unread`,
      },
      [svgIcon(iconName), name],
    );
    button.dataset.folderSlug = folderSlug(folder);

    if (unread > 0) {
      button.append(el('span', { class: 'folder-unread', text: unread > 999 ? '999+' : String(unread) }));
    }

    button.addEventListener('click', () => {
      if (handlers) handlers.onSelectFolder(folder);
    });
    list.append(el('li', {}, [button]));

    if (focusedSlug && focusedSlug === button.dataset.folderSlug) button.focus();
  }
}

/** Create a folder inside the given address. */
async function createFolder(mailboxId) {
  const name = await promptDialog({
    title: 'New folder',
    label: 'Folder name',
    confirmLabel: 'Create',
    hint: 'Nested folders use “Parent/Child”.',
  });
  if (!name) return;
  try {
    const payload = await request(`${API_BASE}/mailboxes/${mailboxId}/folders`, {
      method: 'POST',
      body: { name },
      toast: false,
    });
    const created = foldersOf([payload])[0];
    mutate((state) => {
      if (created && created.id > 0) state.folders = state.folders.concat([created]);
    });
    toastSuccess(`Folder “${name}” created.`);
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : 'The folder could not be created.');
  }
}

/** Rename the folder that is currently open. */
async function renameFolderTo(folder) {
  const name = await promptDialog({
    title: 'Rename folder',
    label: 'Folder name',
    confirmLabel: 'Rename',
    hint: 'Nested folders use “Parent/Child”. INBOX cannot be renamed.',
  });
  if (!name || name === folder.name) return;
  const mailboxId = getState().mailboxId;
  try {
    await request(`${API_BASE}/folders/${folder.id}`, {
      method: 'PATCH',
      body: { name },
      toast: false,
    });
    toastSuccess(`Folder renamed to “${name}”.`);
    await reselectAfterChange(mailboxId, folder.id);
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : 'The folder could not be renamed.');
  }
}

/** Delete the folder that is currently open, after an explicit confirmation. */
async function deleteFolderFrom(folder) {
  const confirmed = await confirmDialog({
    title: 'Delete this folder?',
    message: `“${folder.name}” and everything filed in it are removed. This cannot be undone.`,
    confirmLabel: 'Delete folder',
  });
  if (!confirmed) return;

  const mailboxId = getState().mailboxId;
  try {
    await request(`${API_BASE}/folders/${folder.id}`, { method: 'DELETE', toast: false });
    toastSuccess(`Folder “${folder.name}” deleted.`);
    // The folder that was on screen is gone, so the tree falls back to INBOX.
    await reselectAfterChange(mailboxId, 0);
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : 'The folder could not be deleted.');
  }
}

/**
 * Reload the tree after a rename or a delete and put the selection back.
 *
 * `preferredId` is the folder that should stay selected; `0` (a folder that no
 * longer exists) falls back to INBOX, so the panes never point at a dead id.
 * @param {number} mailboxId
 * @param {number} preferredId
 */
async function reselectAfterChange(mailboxId, preferredId) {
  const folders = await loadFolders(mailboxId);
  const current = getState().folder;
  const wanted =
    (preferredId && folders.find((candidate) => candidate.id === preferredId)) ||
    (current && folders.find((candidate) => candidate.id === current.id)) ||
    folders.find((candidate) => candidate.specialUse === 'inbox') ||
    folders[0] ||
    null;
  if (wanted && handlers) handlers.onSelectFolder(wanted);
}

/**
 * Compare the fields the UI renders, so a folder reload that changes nothing
 * does not trigger a repaint (which would move focus out of the tree).
 * @param {Array<object>} a
 * @param {Array<object>} b
 */
function sameFolders(a, b) {
  if (a.length !== b.length) return false;
  for (let index = 0; index < a.length; index += 1) {
    const left = a[index];
    const right = b[index];
    if (
      left.id !== right.id ||
      left.name !== right.name ||
      left.unseenCount !== right.unseenCount ||
      left.messageCount !== right.messageCount ||
      left.specialUse !== right.specialUse
    ) {
      return false;
    }
  }
  return true;
}

/**
 * Reload the folder tree. On a network failure the previous tree stays on screen
 * and the offline banner appears instead of an empty pane.
 * @param {number} mailboxId
 */
export async function loadFolders(mailboxId) {
  try {
    const payload = await request(`${API_BASE}/mailboxes/${mailboxId}/folders`, { toast: false });
    const folders = foldersOf(payload);
    if (sameFolders(getState().folders, folders)) return getState().folders;
    mutate((state) => {
      state.folders = folders;
    });
    return folders;
  } catch (error) {
    if (error instanceof ApiError && error.network) {
      setHidden(byId('offline-banner'), false);
      return getState().folders;
    }
    toastError(error instanceof ApiError ? error.message : 'Folders could not be loaded.');
    return getState().folders;
  }
}
