/**
 * Hash routing. The address bar always describes what is on screen, so refresh,
 * bookmarking and the browser's back button all work.
 *
 *   #/f/inbox            a folder
 *   #/f/sent/m/4821      a folder with one message open
 *   #/search/invoice     a search inside the current folder
 */

const DEFAULT_FOLDER = 'inbox';

function encode(segment) {
  return encodeURIComponent(segment).replace(/%2F/gi, '/');
}

/**
 * @typedef {{name: 'folder', slug: string, messageId: number, search: string}
 *          |{name: 'search', slug: string, messageId: number, search: string}} Route
 */

/** @returns {Route} */
export function parseHash() {
  const raw = window.location.hash.replace(/^#\/?/, '');
  const parts = raw.split('/').filter((part) => part !== '');
  if (parts.length === 0) return { name: 'folder', slug: DEFAULT_FOLDER, messageId: 0, search: '' };

  if (parts[0] === 'search') {
    const query = parts.length > 1 ? decodeURIComponent(parts.slice(1).join('/')) : '';
    return { name: 'search', slug: DEFAULT_FOLDER, messageId: 0, search: query };
  }

  if (parts[0] === 'f') {
    const slug = parts[1] ? decodeURIComponent(parts[1]) : DEFAULT_FOLDER;
    const messageId = parts[2] === 'm' && parts[3] ? Number.parseInt(parts[3], 10) : 0;
    return {
      name: 'folder',
      slug,
      messageId: Number.isFinite(messageId) ? messageId : 0,
      search: '',
    };
  }

  // Not a route this app knows. The mail client has nowhere else to be, so it shows the
  // inbox — but the address bar must stop claiming otherwise, which is the promise in this
  // file's first paragraph. `#/admin/` is what makes that concrete: it is the *console's*
  // shape (its sections are `#/<section>`, and it lives at `/admin/`), so opening it here
  // showed an inbox under a URL that named a different application. `replaceState` keeps it
  // out of history and fires no `hashchange`, so this cannot loop.
  replaceHash(`/f/${DEFAULT_FOLDER}`);
  return { name: 'folder', slug: DEFAULT_FOLDER, messageId: 0, search: '' };
}

/** Replace the hash without adding a history entry. */
function replaceHash(hash) {
  const next = `#${hash}`;
  if (window.location.hash === next) return;
  window.history.replaceState(null, '', next);
}

function pushHash(hash) {
  const next = `#${hash}`;
  if (window.location.hash === next) return;
  window.location.hash = next;
}

/**
 * @param {string} slug
 * @param {number} [messageId]
 * @param {{replace?: boolean}} [options]
 */
export function goFolder(slug, messageId = 0, options = {}) {
  const hash = `/f/${encode(slug || DEFAULT_FOLDER)}${messageId ? `/m/${messageId}` : ''}`;
  if (options.replace) replaceHash(hash);
  else pushHash(hash);
}

/**
 * @param {string} query
 * @param {{replace?: boolean}} [options]
 */
export function goSearch(query, options = {}) {
  const hash = query ? `/search/${encode(query)}` : `/f/${DEFAULT_FOLDER}`;
  if (options.replace) replaceHash(hash);
  else pushHash(hash);
}

/**
 * A stable, human-readable slug for a folder: its special-use name when it has
 * one, otherwise `folder-<id>`.
 * @param {{id: number, specialUse: string|null}|null} folder
 */
export function folderSlug(folder) {
  if (!folder) return DEFAULT_FOLDER;
  if (folder.specialUse) return folder.specialUse;
  return `folder-${folder.id}`;
}

/**
 * Resolve a slug against the loaded folder list.
 * @param {Array<{id: number, specialUse: string|null}>} folders
 * @param {string} slug
 */
export function folderBySlug(folders, slug) {
  if (!slug) return folders[0] || null;
  const special = folders.find((folder) => folder.specialUse === slug);
  if (special) return special;
  const match = /^folder-(\d+)$/.exec(slug);
  if (match) {
    const id = Number.parseInt(match[1], 10);
    const found = folders.find((folder) => folder.id === id);
    if (found) return found;
  }
  return folders[0] || null;
}

/** Subscribe to hash changes. */
export function onRouteChange(handler) {
  const listener = () => handler(parseHash());
  window.addEventListener('hashchange', listener);
  return () => window.removeEventListener('hashchange', listener);
}
