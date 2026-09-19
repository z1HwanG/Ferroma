/**
 * Admin routing. The hash names the section; each view may append its own
 * parameters, e.g. `#/queue?status=failed` or `#/users?query=alice&offset=50`.
 *
 *   #/dashboard   #/domains   #/users   #/aliases    #/queue
 *   #/logs        #/storage   #/devices #/audit      #/settings   #/setup
 *
 * The order is the order of the sidebar, and it follows specification §36: the
 * System Logs, Storage and Devices sections sit with the other operator surfaces.
 *
 * `group` is the sidebar heading a section sits under, and `icon` names a path
 * set in `icons.js`. Sections are rendered in array order, grouped, so the
 * sidebar reads as four short lists instead of one flat list of twelve.
 */

/** @type {Array<{id: string, label: string, title: string, group: string, icon: string}>} */
export const SECTIONS = [
  { id: 'dashboard', label: 'Dashboard', title: 'Dashboard', group: '', icon: 'dashboard' },
  { id: 'domains', label: 'Domains', title: 'Domains', group: 'Mail', icon: 'domains' },
  { id: 'users', label: 'Users', title: 'Users', group: 'Mail', icon: 'users' },
  { id: 'aliases', label: 'Aliases', title: 'Aliases', group: 'Mail', icon: 'aliases' },
  { id: 'queue', label: 'Mail queue', title: 'Mail queue', group: 'Mail', icon: 'queue' },
  { id: 'logs', label: 'System logs', title: 'System logs', group: 'System', icon: 'logs' },
  { id: 'storage', label: 'Storage', title: 'Storage', group: 'System', icon: 'storage' },
  { id: 'devices', label: 'Devices', title: 'Devices', group: 'System', icon: 'devices' },
  { id: 'tls', label: 'TLS', title: 'TLS', group: 'System', icon: 'tls' },
  { id: 'audit', label: 'Audit log', title: 'Audit log', group: 'Operator', icon: 'audit' },
  { id: 'settings', label: 'Settings', title: 'Settings', group: 'Operator', icon: 'settings' },
  { id: 'setup', label: 'Setup', title: 'First-run setup', group: 'Operator', icon: 'setup' },
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
