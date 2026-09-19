/**
 * Admin console shell: sign-in, section navigation, the boot-time health poll
 * that feeds the sidebar, and the view dispatcher.
 */

import { API_BASE, ApiError, clearTokens, onConnectionChange, request, setTokens, setUnauthorizedHandler } from './api.js';
import { byId, clear, el, setHidden, setText } from './dom.js';
import { icon } from './icons.js';
import { parseHash, go, onRouteChange, SECTIONS } from './router.js';
import { getState, setState } from './store.js';
import { initTheme, setTheme, currentTheme } from './theme.js';
import { toastSuccess } from './toast.js';
import { errorState, loadingState } from './ui.js';

const HEALTH_POLL_MS = 30000;

/** @type {Record<string, () => Promise<{node: Node, cleanup?: () => void}>>} */
const views = {};

/** @type {number} */
let healthTimer = 0;
let booting = false;
/** The queue badge inside the navigation, created with the nav buttons. */
let queueBadge = null;

initTheme();

/* ---------------------------------------------------------------- view slots */

/**
 * Load one view module on demand and hand it the content root.
 * @param {string} section
 * @param {URLSearchParams} params
 */
async function mountView(section, params) {
  const host = byId('view-root');
  const previousCleanup = getState().cleanup;
  setState({ cleanup: null });
  if (previousCleanup) {
    try {
      previousCleanup();
    } catch {
      /* a failing teardown must not block the next section */
    }
  }

  const loader = views[section];
  if (!loader) {
    clear(host);
    host.append(
      errorState(`The section “${section}” is not part of this console.`, []),
    );
    return;
  }

  clear(host);
  host.append(loadingState(`Loading ${section}…`));

  try {
    const view = await loader();
    const rendered = await view(params);
    clear(host);
    host.append(rendered.node);
    setState({ cleanup: rendered.cleanup || null });
    host.focus({ preventScroll: true });
  } catch (error) {
    clear(host);
    const message =
      error instanceof ApiError
        ? error.message
        : error instanceof Error
          ? error.message
          : 'This section could not be loaded.';
    host.append(errorState(message));
  }
}

/* ----------------------------------------------------------------- sections */

views.dashboard = async () => (await import('./views/dashboard.js')).render;
views.domains = async () => (await import('./views/domains.js')).render;
views.users = async () => (await import('./views/users.js')).render;
views.aliases = async () => (await import('./views/aliases.js')).render;
views.queue = async () => (await import('./views/queue.js')).render;
views.logs = async () => (await import('./views/logs.js')).render;
views.storage = async () => (await import('./views/storage.js')).render;
views.devices = async () => (await import('./views/devices.js')).render;
views.tls = async () => (await import('./views/tls.js')).render;
views.audit = async () => (await import('./views/audit.js')).render;
views.settings = async () => (await import('./views/settings.js')).render;
views.setup = async () => (await import('./views/setup.js')).render;

/* --------------------------------------------------------------------- boot */

function start() {
  renderNavigation();
  initNav();
  watchNavBreakpoint();
  wireChrome();

  setUnauthorizedHandler(() => {
    stopHealthPoll();
    clearTokens();
    setState({ user: null });
    showLogin('Your session ended. Sign in again.');
  });

  onConnectionChange((online) => {
    setHidden(byId('offline-banner'), online);
  });

  onRouteChange((route) => {
    setText(byId('topbar-status'), '');
    mountView(route.section, route.params);
  });

  boot().then(async (ok) => {
    if (!ok) return;
    const route = parseHash();
    await mountView(route.section, route.params);
    startHealthPoll();
  });
}

/** @returns {Promise<boolean>} whether a usable session exists */
async function boot() {
  if (booting) return false;
  booting = true;
  try {
    const [me, health, version] = await Promise.all([
      request(`${API_BASE}/auth/me`, { toast: false, retryOn401: false }),
      request(`${API_BASE}/health`, { toast: false, retryOn401: false }).catch(() => null),
      request(`${API_BASE}/version`, { toast: false, retryOn401: false }).catch(() => null),
    ]);
    setState({ user: me, health, version });
    if (me && me.is_admin === false) {
      showLogin('This account is not an administrator.');
      return false;
    }
    setHidden(byId('login-view'), true);
    setHidden(byId('app-view'), false);
    renderAccountMenu();
    renderSidebarMeta();
    return true;
  } catch (error) {
    if (error instanceof ApiError && error.network) {
      setHidden(byId('offline-banner'), false);
      showLogin('The management API could not be reached.');
    } else {
      showLogin('');
    }
    return false;
  } finally {
    booting = false;
  }
}

function showLogin(message) {
  setHidden(byId('app-view'), true);
  setHidden(byId('login-view'), false);
  setText(byId('login-error'), message);
  setHidden(byId('login-error'), message === '');
  const email = byId('login-email');
  window.setTimeout(() => email.focus(), 0);
}

/* ---------------------------------------------------------------- navigation */

function renderNavigation() {
  const list = byId('nav-list');
  clear(list);
  queueBadge = null;
  const current = parseHash().section;
  let group = null;

  for (const section of SECTIONS) {
    // One heading per group, emitted in array order so the sidebar reads as
    // four short lists rather than one flat run of twelve entries.
    if (section.group !== group) {
      group = section.group;
      if (group) {
        list.append(el('li', { class: 'nav-group' }, [el('span', { class: 'nav-group-title', text: group })]));
      }
    }

    const button = el('button', {
      type: 'button',
      class: 'btn nav-button',
      'aria-current': section.id === current ? 'true' : 'false',
      dataset: { section: section.id },
    });
    button.append(icon(section.icon, 'icon nav-icon'));
    button.append(el('span', { class: 'nav-label', text: section.label }));
    if (section.id === 'queue') {
      queueBadge = el('span', { class: 'nav-badge' });
      queueBadge.hidden = true;
      button.append(queueBadge);
    }
    button.addEventListener('click', () => {
      go(section.id);
      closeNav();
    });
    list.append(el('li', {}, [button]));
  }
}

function updateNavCurrent(section) {
  // Selected by data attribute rather than by index: the group headings are
  // siblings, so positional lookup against SECTIONS no longer lines up.
  for (const button of byId('nav-list').querySelectorAll('.nav-button')) {
    button.setAttribute('aria-current', button.dataset.section === section ? 'true' : 'false');
  }
}

function closeNav() {
  byId('app-view').dataset.nav = 'closed';
  byId('nav-toggle').setAttribute('aria-expanded', 'false');
}

/** The width at which the sidebar stops being docked and becomes a drawer. */
const NARROW = '(max-width: 860px)';

/**
 * Put the navigation into the state the current layout is actually in.
 *
 * `data-nav` means two different things on purpose: below 860px it opens the
 * overlapping drawer, above it the sidebar is docked and the same attribute only
 * collapses it. Without this, the attribute starts unset, the toggle's first click
 * merely asserts the state the layout is already in, and the button looks dead —
 * which is exactly how it behaved on every desktop-width window.
 */
function initNav() {
  const narrow = window.matchMedia(NARROW).matches;
  byId('app-view').dataset.nav = narrow ? 'closed' : 'open';
  byId('nav-toggle').setAttribute('aria-expanded', narrow ? 'false' : 'true');
}

/** Re-assert the default when the window crosses the breakpoint. */
function watchNavBreakpoint() {
  window.matchMedia(NARROW).addEventListener('change', initNav);
}

function renderAccountMenu() {
  const user = getState().user;
  setText(byId('account-menu-name'), (user && user.display_name) || 'Administrator');
  setText(byId('account-menu-email'), (user && user.email) || '');
}

function renderSidebarMeta() {
  const { version } = getState();
  const parts = [];
  if (version && version.version) parts.push(`Ferroma ${version.version}`);
  if (version && version.git_sha) parts.push(version.git_sha.slice(0, 7));
  if (version && version.protocol_version !== undefined) parts.push(`protocol ${version.protocol_version}`);
  setText(byId('sidebar-version'), parts.join(' · ') || 'Ferroma');
}

/* -------------------------------------------------------------------- chrome */

function wireChrome() {
  const form = byId('login-form');
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    const email = byId('login-email').value.trim();
    const password = byId('login-password').value;
    setHidden(byId('login-error'), true);
    setHidden(byId('login-email-error'), true);
    setHidden(byId('login-password-error'), true);
    if (email === '') {
      setText(byId('login-email-error'), 'Enter your email address.');
      setHidden(byId('login-email-error'), false);
      byId('login-email').focus();
      return;
    }
    if (password === '') {
      setText(byId('login-password-error'), 'Enter your password.');
      setHidden(byId('login-password-error'), false);
      byId('login-password').focus();
      return;
    }

    const submit = byId('login-submit');
    submit.disabled = true;
    setText(submit, 'Signing in…');
    try {
      const payload = await request(`${API_BASE}/auth/login`, {
        method: 'POST',
        body: { email, password },
        toast: false,
        retryOn401: false,
      });
      if (!payload || !payload.user || payload.user.is_admin === false) {
        showLogin('That account is not an administrator.');
        return;
      }
      setTokens(payload);
      byId('login-password').value = '';
      toastSuccess('Signed in.');
      const ok = await boot();
      if (ok) {
        await mountView(parseHash().section, parseHash().params);
        startHealthPoll();
      }
    } catch (error) {
      const message =
        error instanceof ApiError && error.status === 401
          ? 'Wrong address or password.'
          : error instanceof ApiError && error.status === 429
            ? error.message
            : error instanceof ApiError && error.network
              ? 'The management API could not be reached.'
              : 'Sign-in failed.';
      showLogin(message);
    } finally {
      submit.disabled = false;
      setText(submit, 'Sign in');
    }
  });

  byId('nav-toggle').addEventListener('click', () => {
    const app = byId('app-view');
    const open = app.dataset.nav === 'open';
    app.dataset.nav = open ? 'closed' : 'open';
    byId('nav-toggle').setAttribute('aria-expanded', open ? 'false' : 'true');
  });

  byId('sidebar-close').addEventListener('click', closeNav);

  byId('theme-toggle').addEventListener('click', () => {
    const next = currentTheme() === 'dark' ? 'light' : 'dark';
    setTheme(next);
  });

  byId('account-button').addEventListener('click', (event) => {
    event.stopPropagation();
    const menu = byId('account-menu');
    const open = menu.hidden;
    setHidden(menu, !open);
    byId('account-button').setAttribute('aria-expanded', open ? 'true' : 'false');
    if (open) {
      const first = menu.querySelector('.menu-item');
      if (first instanceof HTMLElement) first.focus();
    }
  });

  byId('account-logout').addEventListener('click', async () => {
    setHidden(byId('account-menu'), true);
    try {
      await request(`${API_BASE}/auth/logout`, { method: 'POST', toast: false, retryOn401: false });
    } catch (error) {
      if (!(error instanceof ApiError)) throw error;
    }
    clearTokens();
    stopHealthPoll();
    setState({ user: null });
    showLogin('');
    toastSuccess('Signed out.');
  });

  document.addEventListener('click', (event) => {
    const menu = byId('account-menu');
    if (menu.hidden) return;
    const target = event.target;
    if (target instanceof Element && (target.closest('#account-menu') || target.closest('#account-button'))) return;
    setHidden(menu, true);
    byId('account-button').setAttribute('aria-expanded', 'false');
  });

  document.addEventListener('keydown', (event) => {
    if (event.key !== 'Escape') return;
    const menu = byId('account-menu');
    if (!menu.hidden) {
      setHidden(menu, true);
      byId('account-button').setAttribute('aria-expanded', 'false');
      byId('account-button').focus();
    }
  });

  byId('offline-retry').addEventListener('click', async () => {
    const ok = await boot();
    if (ok) {
      const route = parseHash();
      await mountView(route.section, route.params);
      startHealthPoll();
    }
  });
}

/* --------------------------------------------------------------- health poll */

function startHealthPoll() {
  stopHealthPoll();
  const tick = async () => {
    updateNavCurrent(parseHash().section);
    try {
      const health = await request(`${API_BASE}/health`, { toast: false, retryOn401: false });
      setState({ health });
      renderHealthStatus(health);
    } catch (error) {
      if (error instanceof ApiError && error.network) setHidden(byId('offline-banner'), false);
      setText(byId('topbar-status'), 'health unavailable');
    }
  };
  tick();
  healthTimer = window.setInterval(tick, HEALTH_POLL_MS);
}

function stopHealthPoll() {
  if (healthTimer) window.clearInterval(healthTimer);
  healthTimer = 0;
}

/** @param {Record<string, unknown>|null} health */
function renderHealthStatus(health) {
  if (!health) {
    setText(byId('topbar-status'), '');
    return;
  }
  const queue = health.queue || {};
  const depth = Number(queue.pending || 0) + Number(queue.retry || 0);
  const failed = Number(queue.failed || 0);
  setText(
    byId('topbar-status'),
    `${health.status || 'unknown'} · queue ${depth}${failed ? ` · ${failed} failed` : ''}`,
  );
  if (queueBadge) {
    queueBadge.hidden = failed === 0;
    queueBadge.textContent = failed > 999 ? '999+' : String(failed);
  }
}

/* --------------------------------------------------------------------- start */

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', start, { once: true });
} else {
  start();
}
