/**
 * Admin routing. The hash names the section; each view may append its own
 * parameters, e.g. `#/queue?status=failed` or `#/users?query=alice&offset=50`.
 *
 *   #/dashboard   #/domains   #/users   #/aliases   #/queue
 *   #/audit       #/settings  #/setup
 */

/** @type {Array<{id: string, label: string, title: string}>} */
export const SECTIONS = [
  { id: 'dashboard', label: 'Dashboard', title: 'Dashboard' },
  { id: 'domains', label: 'Domains', title: 'Domains' },
  { id: 'users', label: 'Users', title: 'Users' },
  { id: 'aliases', label: 'Aliases', title: 'Aliases' },
  { id: 'queue', label: 'Mail queue', title: 'Mail queue' },
  { id: 'audit', label: 'Audit log', title: 'Audit log' },
  { id: 'settings', label: 'Settings', title: 'Settings' },
  { id: 'setup', label: 'Setup', title: 'First-run setup' },
];

/**
 * @typedef {{section: string, params: URLSearchParams}} Route
 * @returns {Route}
 */
export function parseHash() {
  const raw = window.location.hash.replace(/^#\/?/, '');
  const [path, search] = raw.split('?');
  const section = (path || 'dashboard').split('/')[0].toLowerCase();
  const known = SECTIONS.some((candidate) => candidate.id === section);
  return {
    section: known ? section : 'dashboard',
    params: new URLSearchParams(search || ''),
  };
}

/**
 * @param {string} section
 * @param {Record<string, string|number|undefined|null>} [params]
 * @param {{replace?: boolean}} [options]
 */
export function go(section, params = {}, options = {}) {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null || value === '') continue;
    search.set(key, String(value));
  }
  const query = search.toString();
  const hash = `#/${section}${query ? `?${query}` : ''}`;
  if (window.location.hash === hash) return;
  if (options.replace) window.history.replaceState(null, '', hash);
  else window.location.hash = hash;
}

/** Subscribe to hash changes. */
export function onRouteChange(handler) {
  const listener = () => handler(parseHash());
  window.addEventListener('hashchange', listener);
  return () => window.removeEventListener('hashchange', listener);
}
