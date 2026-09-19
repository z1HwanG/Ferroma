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

import { t } from '../shared/i18n.js';

/** @type {Array<{id: string, label: string, title: string, group: string, icon: string}>} */
export const SECTIONS = [
  { id: 'dashboard', label: t('Dashboard'), title: t('Dashboard'), group: '', icon: 'dashboard' },
  { id: 'domains', label: t('Domains'), title: t('Domains'), group: t('Mail'), icon: 'domains' },
  { id: 'users', label: t('Users'), title: t('Users'), group: t('Mail'), icon: 'users' },
  { id: 'aliases', label: t('Aliases'), title: t('Aliases'), group: t('Mail'), icon: 'aliases' },
  { id: 'queue', label: t('Mail queue'), title: t('Mail queue'), group: t('Mail'), icon: 'queue' },
  { id: 'logs', label: t('System logs'), title: t('System logs'), group: t('System'), icon: 'logs' },
  { id: 'storage', label: t('Storage'), title: t('Storage'), group: t('System'), icon: 'storage' },
  { id: 'devices', label: t('Devices'), title: t('Devices'), group: t('System'), icon: 'devices' },
  { id: 'tls', label: t('TLS certificates'), title: t('TLS certificates'), group: t('System'), icon: 'tls' },
  { id: 'audit', label: t('Audit log'), title: t('Audit log'), group: t('Operator'), icon: 'audit' },
  { id: 'settings', label: t('Settings'), title: t('Settings'), group: t('Operator'), icon: 'settings' },
  { id: 'setup', label: t('Setup'), title: t('First-run setup'), group: t('Operator'), icon: 'setup' },
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
