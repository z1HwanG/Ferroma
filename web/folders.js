/**
 * Folder pane: the send-from address picker and the folder tree with unread
 * counts. Special-use folders are identified by `special_use`, never by name, so
 * a localised or renamed "Sent" folder still gets the right icon.
 */

import { API_BASE, ApiError, request } from '../shared/api.js';
import { foldersOf } from '../shared/data.js';
import { byId, clear, el, labelWithTitle, setHidden, svgIcon } from '../shared/dom.js';
import { confirmDialog, promptDialog } from '../shared/modal.js';
import { t } from '../shared/i18n.js';
import { folderSlug } from './router.js';
import { getState, mutate } from './store.js';
import { toastError, toastSuccess } from '../shared/toast.js';

const ICON_FOR_SPECIAL = {
  inbox: 'inbox',
  sent: 'sent',
  drafts: 'drafts',
  trash: 'trash',
  junk: 'junk',
  archive: 'archive',
};

/**
 * The name a standard folder is *shown* under.
 *
 * `folder.name` is IMAP data — it is what `SELECT`, `RENAME` and `APPEND` must send —
 * so it is never translated. The display name of a standard folder is a UI string
 * though, and a Chinese interface listing `INBOX`, `Sent` and `Trash` is half
 * translated. The special-use slug decides, which is why `data.js` derives `inbox`
 * from the INBOX name; a folder with no standard meaning keeps its own name.
 *
 * Each label is written as a literal inside `t(...)` rather than looked up in a table,
 * so the coverage rule in `tools/check.mjs` can see it. A label held in a table is
 * invisible to a static reader, and an untranslated one would pass every check.
 *
 * @param {{name: string, specialUse: string|null}} folder
 */
/* Exported so the list header names a folder the way the tree beside it does. */
export function folderLabel(folder) {
  switch (folder.specialUse) {
    case 'inbox':
      return t('Inbox');
    case 'sent':
      return t('Sent');
    case 'drafts':
      return t('Drafts');
    case 'trash':
      return t('Trash');
    case 'junk':
      return t('Junk');
    case 'archive':
      return t('Archive');
    default:
      // A nested folder's IMAP name is the whole path (`Projects/2026`). Showing that in
      // the tree repeats the parent on every child; the indentation already says where
      // the row sits, so the label is the last segment. `folder.name` stays the path for
      // every operation that has to send it.
      return leafName(folder.name);
  }
}

/** The last segment of an IMAP folder path. */
function leafName(name) {
  const text = String(name || '');
  const cut = text.lastIndexOf('/');
  return cut >= 0 ? text.slice(cut + 1) || text : text;
}

/** @type {{onSelectFolder: (folder: object) => void, onSelectMailbox: (mailboxId: number) => void}|null} */
let handlers = null;

/**
 * The order the reader has dragged their own folders into, per address.
 *
 * IMAP has no notion of folder order, so this is a *client* preference and is kept where
 * the other client preferences live. A folder that is not in the list — new, or added by
 * another client — sorts after the ones that are, by creation, which is why an untouched
 * mailbox already reads in the order things were made.
 */
const ORDER_KEY_PREFIX = 'ferroma.folderOrder.';

function readFolderOrder(mailboxId) {
  try {
    const raw = window.localStorage.getItem(`${ORDER_KEY_PREFIX}${mailboxId}`);
    const parsed = raw ? JSON.parse(raw) : [];
    return Array.isArray(parsed) ? parsed.map(Number).filter(Number.isFinite) : [];
  } catch {
    return [];
  }
}

function writeFolderOrder(mailboxId, ids) {
  try {
    window.localStorage.setItem(`${ORDER_KEY_PREFIX}${mailboxId}`, JSON.stringify(ids));
  } catch {
    /* storage unavailable: the order lasts for this page only */
  }
}

/** The id the pointer is currently dragging, or 0. */
let draggingFolderId = 0;

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

  // The tree's own background lifts a folder out of its parent, and rows stop their drops
  // from reaching it — but "drop it on the empty space below the tree" is not something a
  // reader can be expected to discover, and with a long tree there is often no space left
  // to aim at. So while a *nested* folder is being dragged, an explicit target appears at
  // the end of the tree. Both work; this one is visible.
  const list = byId('folder-list');
  list.addEventListener('dragover', (event) => {
    if (!draggingFolderId || parentOfFolder(draggingFolderId) === null) return;
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = 'move';
    list.classList.add('drop-root');
  });
  list.addEventListener('dragleave', (event) => {
    if (event.target === list) list.classList.remove('drop-root');
  });
  list.addEventListener('drop', (event) => {
    event.preventDefault();
    list.classList.remove('drop-root');
    const dragged = draggedId(event);
    draggingFolderId = 0;
    hideRootDropZone();
    clearDropMarks();
    if (!dragged || parentOfFolder(dragged) === null) return;
    reparentFolder(dragged, null);
  });

  newFolder.addEventListener('click', () => {
    const state = getState();
    if (!state.mailboxId) {
      toastError(t('Choose a send-from address first.'));
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
      toastError(t('Open a folder first.'));
      return;
    }
    renameFolderTo(folder);
  });

  // The explicit path to the same operation the drag performs. It exists because "drag it
  // out of its parent" is a gesture with nowhere obvious to aim at, and a capability that
  // can only be reached by dragging is a capability most readers never find.
  const rootFolder = byId('root-folder-button');
  rootFolder.addEventListener('click', () => {
    const folder = getState().folder;
    if (!folder) {
      toastError(t('Open a folder first.'));
      return;
    }
    if (parentOfFolder(folder.id) === null) {
      toastError(t('This folder is already at the top level.'));
      return;
    }
    reparentFolder(folder.id, null);
  });

  deleteFolder.addEventListener('click', () => {
    const folder = getState().folder;
    if (!folder) {
      toastError(t('Open a folder first.'));
      return;
    }
    deleteFolderFrom(folder);
  });
}

/**
 * Repaint the send-from address picker.
 *
 * An account with one address has nothing to pick: the control was a disabled `<select>`
 * whose "…(primary)" suffix was truncated by the pane, which reads as a broken widget
 * rather than as a label. One address is shown as a plain line; two or more keep the real
 * picker, which is what chooses whose folders are on screen and which address new mail is
 * sent from.
 */
export function renderMailboxes() {
  const select = byId('mailbox-select');
  const identity = byId('mailbox-identity');
  const state = getState();

  const single = state.mailboxes.length === 1 ? state.mailboxes[0] : null;
  select.hidden = Boolean(single);
  identity.hidden = !single;
  if (single) {
    identity.textContent = single.address;
    identity.title = single.address;
  }

  clear(select);
  for (const mailbox of state.mailboxes) {
    // The addresses alone. Appending "(primary)" truncated in a 244px pane — the one
    // place it was useful to know is the account drawer, which marks it in its own table.
    select.append(el('option', { value: String(mailbox.id), text: mailbox.address }));
  }
  select.value = String(state.mailboxId);
  select.title = state.mailboxes.map((mailbox) => mailbox.address).join('\n');


  // Writing needs a `From`. An account with no address can open the dialog but cannot
  // send, so the button says so instead of offering a form that fails at the end.
  const compose = byId('compose-button');
  compose.disabled = state.mailboxes.length === 0;
  compose.title = state.mailboxes.length === 0
    ? t('Add an address to this account before composing.')
    : '';
}

/** Repaint the folder tree from state. */
export function renderFolders() {
  const list = byId('folder-list');
  const state = getState();

  const previouslyFocused = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const focusedSlug = previouslyFocused && previouslyFocused.dataset ? previouslyFocused.dataset.folderSlug : null;

  clear(list);

  if (state.folders.length === 0) {
    // Two different situations look identical in a folder pane. An account with no
    // address has nothing to open and nothing the user can fix from here, while an
    // address whose folders merely failed to load can retry. Saying which one it is
    // is the difference between a support ticket and a dead end.
    list.append(
      el('li', {
        class: 'empty',
        text:
          state.mailboxes.length === 0
            ? t('This account has no email address yet. An administrator has to add one before you can send or receive mail.')
            : t('No folders yet.'),
      }),
    );
    return;
  }

  const order = readFolderOrder(state.mailboxId);
  const roots = folderForest(state.folders, order);

  /** Render one level, nesting a child list inside its parent's row. */
  const appendLevel = (nodes, host, parentId = null) => {
    for (const node of nodes) {
      const folder = node.folder;
      const active = Boolean(state.folder) && state.folder.id === folder.id;
      const iconName = folder.specialUse ? ICON_FOR_SPECIAL[folder.specialUse] || 'folder' : 'folder';
      const unread = folder.unseenCount;

      const name = el('span', { class: 'folder-name' });
      labelWithTitle(name, folderLabel(folder), active ? t('Current folder') : t('Folder'));

      const button = el(
        'button',
        {
          type: 'button',
          class: 'btn folder-button',
          'aria-current': active ? 'true' : 'false',
          'aria-label': active
            ? t('{name}, {count} messages, {unread} unread, current folder', {
                name: folderLabel(folder),
                count: folder.messageCount,
                unread,
              })
            : t('{name}, {count} messages, {unread} unread', {
                name: folderLabel(folder),
                count: folder.messageCount,
                unread,
              }),
          title: t('{name} — {count} messages, {unread} unread', {
            name: folderLabel(folder),
            count: folder.messageCount,
            unread,
          }),
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

      // Drag to reorder, within one parent. The tree nests by `parent_id`, so moving a
      // row past a folder from another level would not change where it lives — it would
      // only look like it had. A drop on a sibling is the gesture that has a meaning.
      if (!folder.specialUse) {
        button.draggable = true;
        button.addEventListener('dragstart', (event) => {
          draggingFolderId = folder.id;
          button.classList.add('is-dragging');
          // Shown for every folder drag, not only nested ones: a reader dragging a
          // top-level folder still learns where the target is.
          showRootDropZone();
          if (event.dataTransfer) {
            event.dataTransfer.effectAllowed = 'move';
            event.dataTransfer.setData(FOLDER_DRAG_TYPE, String(folder.id));
            event.dataTransfer.setData('text/plain', folderLabel(folder));
          }
        });
        button.addEventListener('dragend', () => {
          draggingFolderId = 0;
          button.classList.remove('is-dragging');
          hideRootDropZone();
          clearDropMarks();
        });
        button.addEventListener('dragover', (event) => {
          if (!draggingFolderId || draggingFolderId === folder.id) return;
          // Dropping a folder inside itself (or inside one of its own children) is a cycle
          // the server refuses; saying so here with the pointer saves a round trip.
          if (isSelfOrDescendant(draggingFolderId, folder.id)) {
            if (event.dataTransfer) event.dataTransfer.dropEffect = 'none';
            return;
          }
          event.preventDefault();
          // …and stop here: the tree's own handler is the "drop on the empty space" target,
          // and letting the event bubble would light up the whole pane while the pointer is
          // over a row.
          event.stopPropagation();
          document.getElementById('folder-list')?.classList.remove('drop-root');
          if (event.dataTransfer) event.dataTransfer.dropEffect = 'move';
          const zone = dropZone(button, event.clientY);
          // Two gestures, two pictures: the middle of a row means "put it inside", the
          // edges mean "put it here, in this order". Judging by the *parent* instead — as
          // this first did — made two folders at the same level always reorder, so dragging
          // one onto another could never nest it.
          button.classList.remove('drop-target', 'drop-after', 'drop-into');
          button.classList.add(zone === 'inside' ? 'drop-into' : zone === 'before' ? 'drop-target' : 'drop-after');
        });
        button.addEventListener('dragleave', () => {
          button.classList.remove('drop-target', 'drop-after', 'drop-into');
        });
        button.addEventListener('drop', (event) => {
          event.preventDefault();
          // The tree itself is a drop zone (move to the top level); a drop on a row must
          // not also count as a drop on the tree.
          event.stopPropagation();
          const zone = dropZone(button, event.clientY);
          button.classList.remove('drop-target', 'drop-after', 'drop-into');
          const dragged = draggedId(event);
          if (!dragged || dragged === folder.id) return;
          if (isSelfOrDescendant(dragged, folder.id)) return;
          dropFolder(dragged, folder.id, zone);
        });
      }

      const row = el('li', {}, [button]);
      if (node.children.length > 0) {
        // A real nested list rather than an indent: the indentation comes from the
        // structure, so a screen reader announces the child as belonging to its parent
        // and no inline style is needed to line the levels up.
        row.append(el('ul', { class: 'folder-children' }, []));
        appendLevel(node.children, row.lastElementChild, folder.id);
      }
      host.append(row);

      if (focusedSlug && focusedSlug === button.dataset.folderSlug) button.focus();
    }
  };

  appendLevel(roots, list);

  // "Move to the top level" only means something for a folder that has a parent, and this
  // is the render that runs whenever the open folder changes — `renderMailboxes` only runs
  // when the *address* changes, so a button enabled there stayed stale.
  const current = state.folder;
  byId('root-folder-button').disabled =
    !current || current.specialUse || parentOfFolder(current.id) === null;
}

/**
 * Put the "move to the top level" target at the end of the tree.
 *
 * The lookups here are deliberately `document.getElementById`, not the module's `byId`:
 * `byId` is the strict one ("this element must exist, and a missing one is a bug"), and
 * this element is the opposite — it exists only while a nested folder is being dragged, so
 * asking for it is a question, not an assertion. Using `byId` for it threw
 * `missing element #folder-root-drop` on the first drag and aborted the handler.
 */
/**
 * The folder id travels under our own drag type.
 *
 * `text/plain` is the generic type, and a browser fills it with the *selected text* when a
 * drag starts from a selection rather than from the element — so a folder named `451` arrived
 * as the string "451", was read as a folder id, and the move answered `no such folder`. The
 * id is also checked against the folders actually on screen, so nothing that is not a folder
 * can be treated as one.
 */
const FOLDER_DRAG_TYPE = 'text/x-ferroma-folder';

/**
 * The folder being dragged, according to the event itself.
 *
 * `dragstart` stores the id in the drag data, which is what the *browser* carries; the
 * module-level `draggingFolderId` is only a fallback — for the synthetic drags a test harness
 * sends, and for a drag whose data was lost. Reading the event matters because `dragend` does
 * not always arrive (a drop outside the window, a cancelled drag), and a stale id from the
 * last drag makes the next drop move the wrong folder.
 */
function draggedId(event) {
  const folders = getState().folders;
  const known = (id) => folders.some((folder) => folder.id === id);
  const carried = Number(event.dataTransfer ? event.dataTransfer.getData(FOLDER_DRAG_TYPE) : NaN);
  if (Number.isFinite(carried) && known(carried)) return carried;
  return known(draggingFolderId) ? draggingFolderId : 0;
}

function showRootDropZone() {
  const list = byId('folder-list');
  if (document.getElementById('folder-root-drop')) return;
  const zone = el('li', { class: 'folder-root-drop', id: 'folder-root-drop' }, [
    el('span', { text: t('Move to the top level') }),
  ]);
  zone.addEventListener('dragover', (event) => {
    if (!draggingFolderId) return;
    event.preventDefault();
    event.stopPropagation();
    if (event.dataTransfer) event.dataTransfer.dropEffect = 'move';
    zone.classList.add('drop-root');
  });
  zone.addEventListener('dragleave', () => zone.classList.remove('drop-root'));
  zone.addEventListener('drop', (event) => {
    event.preventDefault();
    event.stopPropagation();
    const dragged = draggedId(event);
    draggingFolderId = 0;
    hideRootDropZone();
    clearDropMarks();
    if (dragged) reparentFolder(dragged, null);
  });
  list.append(zone);
}

/** Take it away again. */
function hideRootDropZone() {
  const zone = document.getElementById('folder-root-drop');
  if (zone) zone.remove();
}

/** Remove every drop hint, after a drag ends or a drop lands. */
function clearDropMarks() {
  for (const node of document.querySelectorAll(
    '.folder-button.drop-target, .folder-button.drop-into, .folder-button.is-dragging, .folder-list.drop-root',
  )) {
    node.classList.remove('drop-target', 'drop-into', 'is-dragging', 'drop-root');
  }
}

/**
 * Whether `candidate` is `id` itself or one of its descendants.
 *
 * The folder tree is a `parent_id` chain, so walking up from the candidate is the shortest
 * proof — and the server refuses the move anyway; this only keeps the pointer from
 * promising something that will fail.
 */
function isSelfOrDescendant(id, candidate) {
  if (id === candidate) return true;
  const folders = getState().folders;
  let cursor = candidate;
  const seen = new Set();
  while (cursor && !seen.has(cursor)) {
    seen.add(cursor);
    if (cursor === id) return true;
    const row = folders.find((folder) => folder.id === cursor);
    cursor = row ? Number(row.parent) || 0 : 0;
  }
  return false;
}

/**
 * The parent of one folder, by the tree the server sent.
 *
 * `null` means "no parent" — the top level — and it has to stay `null`. `Number(null)` is
 * `0`, which is finite, so the first version of this reported every top-level folder as
 * having parent `0`, and an edge drop onto one sent `PATCH /folders/25 {"parent_id":0}`:
 * the server looked up folder zero and answered `no such folder`. A folder id is positive;
 * anything else is absence.
 */
function parentOfFolder(id) {
  const found = getState().folders.find((folder) => folder.id === id);
  if (!found || found.parent === null || found.parent === undefined) return null;
  const parent = Number(found.parent);
  return Number.isFinite(parent) && parent > 0 ? parent : null;
}

/**
 * Put `draggedId` where `targetId` is, and remember the order.
 *
 * The list that gets stored is the *rendered* order, so the first drag records the order
 * the reader was already looking at — including the folders they never touched — and a
 * later reload reproduces it exactly.
 */
/**
 * Which of a row's three zones the pointer is in.
 *
 * The outer quarters insert before or after the row; the middle half drops *into* it. This
 * is the gesture file managers have used for decades, and it is the only thing that lets a
 * drop on a sibling mean both "nest it here" and "put it above this one".
 *
 * @param {Element} row
 * @param {number} pointerY
 * @returns {'before'|'inside'|'after'}
 */
function dropZone(row, pointerY) {
  const box = row.getBoundingClientRect();
  const offset = box.height > 0 ? (pointerY - box.top) / box.height : 0.5;
  if (offset < 0.25) return 'before';
  if (offset > 0.75) return 'after';
  return 'inside';
}

/** Handle a drop on one row, whichever of the three zones it landed in. */
async function dropFolder(draggedId, targetId, zone) {
  if (zone === 'inside') {
    await reparentFolder(draggedId, targetId);
    return;
  }
  // An edge drop is about order, and order is a client preference — but it is the order
  // *within a parent*, so a drop from another level moves first and orders second.
  const targetParent = parentOfFolder(targetId);
  if (parentOfFolder(draggedId) !== targetParent) {
    const moved = await reparentFolder(draggedId, targetParent, { quiet: true });
    if (!moved) return;
  }
  reorderFolder(draggedId, targetId, zone);
  renderFolders();
}

/** Put `draggedId` immediately before or after `targetId` in the stored order. */
function reorderFolder(draggedId, targetId, zone) {
  const state = getState();
  const order = readFolderOrder(state.mailboxId);
  const flat = flattenCustomFolders(folderForest(state.folders, order));
  const current = flat.length ? flat : state.folders.filter((f) => !f.specialUse).map((f) => f.id);
  const from = current.indexOf(draggedId);
  const to = current.indexOf(targetId);
  if (from < 0 || to < 0) return;
  current.splice(from, 1);
  const at = current.indexOf(targetId) + (zone === 'after' ? 1 : 0);
  current.splice(at, 0, draggedId);
  writeFolderOrder(state.mailboxId, current);
}

/**
 * Move a folder under another one (or back to the top level).
 *
 * A folder's IMAP name is its path, so this is a rename the *server* performs: it rewrites
 * the folder and every descendant, moves the Maildir directories, and refuses a move into
 * the folder's own subtree. The tree is re-read afterwards rather than patched locally —
 * names changed, and a client-side guess at the new paths is how a tree ends up disagreeing
 * with what the server would answer.
 */
async function reparentFolder(folderId, parentId, options = {}) {
  const state = getState();
  try {
    await request(`${API_BASE}/folders/${folderId}`, {
      method: 'PATCH',
      body: { parent_id: parentId },
      toast: false,
    });
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : t('The folder could not be moved.'));
    return false;
  }
  await loadFolders(state.mailboxId);
  // The folder on screen may be the one that just moved: its *name* is its path, so the
  // copy in state would keep spelling the old one until the next navigation.
  const open = getState().folder;
  if (open) {
    const fresh = getState().folders.find((folder) => folder.id === open.id);
    if (fresh) mutate((draft) => {
      draft.folder = fresh;
    });
  }
  if (!options.quiet) {
    renderFolders();
    toastSuccess(
      parentId === null
        ? t('Moved to the top level.')
        : t('Moved inside “{name}”.', { name: folderLabelById(parentId) }),
    );
  }
  return true;
}

/** The label of one folder, for a message about it. */
function folderLabelById(id) {
  const folder = getState().folders.find((entry) => entry.id === id);
  return folder ? folderLabel(folder) : t('folder');
}

/** The custom folders of a forest, in the order the tree draws them. */
function flattenCustomFolders(roots) {
  const out = [];
  const walk = (nodes) => {
    for (const node of nodes) {
      if (!node.folder.specialUse) out.push(node.folder.id);
      walk(node.children);
    }
  };
  walk(roots);
  return out;
}

/**
 * Arrange folders as a forest, so the tree can nest them.
 *
 * A folder's parent comes from `parent_id` when the server sent one, and otherwise
 * from the `/` in its own name — the form every version before `parent_id` was
 * populated used, and the form IMAP itself defines. Both are walked, a folder whose
 * parent is missing from this mailbox is treated as a root, and a cycle is broken
 * rather than followed: no folder may disappear from the pane because its ancestry is
 * odd.
 *
 * @param {Array<object>} folders
 * @returns {Array<{folder: object, children: Array<object>}>}
 */
function folderForest(folders, order = []) {
  const byId = new Map(folders.map((folder) => [folder.id, folder]));
  const byName = new Map(folders.map((folder) => [folder.name, folder]));
  const parentOf = new Map();

  for (const folder of folders) {
    const explicit = Number(folder.parent);
    if (Number.isFinite(explicit) && byId.has(explicit) && explicit !== folder.id) {
      parentOf.set(folder.id, explicit);
      continue;
    }
    const slash = folder.name.lastIndexOf('/');
    const named = slash > 0 ? byName.get(folder.name.slice(0, slash)) : null;
    parentOf.set(folder.id, named && named.id !== folder.id ? named.id : null);
  }

  // Special folders keep their fixed order (inbox, drafts, sent …). A folder the reader
  // created sorts where they put it, and otherwise by creation — not by name, which
  // shuffled a new folder into the middle of the tree the moment it was made.
  const rank = new Map(order.map((id, index) => [id, index]));
  const compare = (a, b) => {
    if (a.sortKey !== b.sortKey) return a.sortKey - b.sortKey;
    const left = rank.has(a.id) ? rank.get(a.id) : order.length + a.id;
    const right = rank.has(b.id) ? rank.get(b.id) : order.length + b.id;
    if (left !== right) return left - right;
    return a.id - b.id;
  };

  const children = new Map();
  for (const folder of folders.slice().sort(compare)) {
    const parentId = parentOf.get(folder.id) || null;
    if (!children.has(parentId)) children.set(parentId, []);
    children.get(parentId).push(folder);
  }

  const attached = new Set();
  const build = (parentId) => {
    const out = [];
    for (const folder of children.get(parentId) || []) {
      if (attached.has(folder.id)) continue;
      attached.add(folder.id);
      out.push({ folder, children: build(folder.id) });
    }
    return out;
  };

  const roots = build(null);
  // A folder inside a parent cycle is still shown, at the top level, rather than lost.
  for (const folder of folders.slice().sort(compare)) {
    if (!attached.has(folder.id)) {
      attached.add(folder.id);
      roots.push({ folder, children: build(folder.id) });
    }
  }
  return roots;
}

/** Create a folder inside the given address. */
async function createFolder(mailboxId) {
  const state = getState();
  // When a custom folder is open, offer it as the parent, so "Child" under "Projects"
  // is one keystroke rather than a path the operator has to spell from memory.
  const open = state.folder;
  const parentPath = open && !open.specialUse ? open.name : '';
  const name = await promptDialog({
    title: t('New folder'),
    label: t('Folder name'),
    confirmLabel: t('Create'),
    value: parentPath ? `${parentPath}/` : '',
    hint: t('Nested folders use “Parent/Child”.'),
  });
  if (!name) return;
  try {
    const payload = await request(`${API_BASE}/mailboxes/${mailboxId}/folders`, {
      method: 'POST',
      body: { name },
      toast: false,
    });
    // The response is the leaf. A `Parent/Child` name may also have created the parent
    // rows server-side, so the tree is reloaded rather than patched with one folder.
    await loadFolders(mailboxId);
    renderFolders();
    const created = foldersOf([payload])[0];
    const label = created && created.name ? created.name : name;
    toastSuccess(t('Folder “{name}” created.', { name: label }));
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : t('The folder could not be created.'));
  }
}

/** Rename the folder that is currently open. */
async function renameFolderTo(folder) {
  const name = await promptDialog({
    title: t('Rename folder'),
    label: t('Folder name'),
    confirmLabel: t('Rename'),
    hint: t('Nested folders use “Parent/Child”. INBOX cannot be renamed.'),
  });
  if (!name || name === folder.name) return;
  const mailboxId = getState().mailboxId;
  try {
    await request(`${API_BASE}/folders/${folder.id}`, {
      method: 'PATCH',
      body: { name },
      toast: false,
    });
    toastSuccess(t('Folder renamed to “{name}”.', { name }));
    await reselectAfterChange(mailboxId, folder.id);
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : t('The folder could not be renamed.'));
  }
}

/** Delete the folder that is currently open, after an explicit confirmation. */
async function deleteFolderFrom(folder) {
  const confirmed = await confirmDialog({
    title: t('Delete this folder?'),
    message: t('“{name}” and everything filed in it are removed. This cannot be undone.', {
      name: folderLabel(folder),
    }),
    confirmLabel: t('Delete folder'),
  });
  if (!confirmed) return;

  const mailboxId = getState().mailboxId;
  try {
    await request(`${API_BASE}/folders/${folder.id}`, { method: 'DELETE', toast: false });
    toastSuccess(t('Folder “{name}” deleted.', { name: folderLabel(folder) }));
    // The folder that was on screen is gone, so the tree falls back to INBOX.
    await reselectAfterChange(mailboxId, 0);
  } catch (error) {
    toastError(error instanceof ApiError ? error.message : t('The folder could not be deleted.'));
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
    toastError(error instanceof ApiError ? error.message : t('Folders could not be loaded.'));
    return getState().folders;
  }
}
