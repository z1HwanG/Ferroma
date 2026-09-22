/**
 * Admin console shell: sign-in, section navigation, the boot-time health poll
 * that feeds the sidebar, and the view dispatcher.
 */

import { API_BASE, ApiError, clearTokens, onConnectionChange, request, setTokens, setUnauthorizedHandler } from '../shared/api.js';
import { byId, clear, el, setHidden, setText } from '../shared/dom.js';
import { t } from '../shared/i18n.js';
import { icon } from './icons.js';
import { parseHash, go, onRouteChange, SECTIONS } from './router.js';
import { getState, setState } from './store.js';
import { initTheme, onThemeChange, toggleTheme } from '../shared/theme.js';
import { toastSuccess } from '../shared/toast.js';
import { errorState, loadingState } from './ui.js';

const HEALTH_POLL_MS = 30000;

/** @type {Record<string, () => Promise<{node: Node, cleanup?: () => void}>>} */
const views = {};

/** @type {number} */
let healthTimer = 0;
let booting = false;
/** Whether the console is showing the first-run wizard because no administrator exists. */
let setupMode = false;
/** Whether the console is showing the database step because the server has none yet. */
let bootstrapMode = false;
/** The queue badge inside the navigation, created with the nav buttons. */
let queueBadge = null;
/** Which mount owns the content pane; see `mountView`. */
let mountGeneration = 0;

initTheme();

/* --------------------------------------------------------------- never blank */

/**
 * Tell `shared/diag.js` that a view is on screen, so its watchdog stands down.
 *
 * The guard itself cannot live here: it has to survive this module failing to load, which
 * is why it is a separate script with no imports. A page that reached the sign-in panel,
 * the console, or the first-run wizard is not blank, and that is all it needs to know.
 */
function viewShown() {
  if (typeof window.__ferromaReady === 'function') window.__ferromaReady();
}

/**
 * Keep the theme toggle's pressed state in step with the scheme in force.
 *
 * The glyph is chosen by CSS from `html[data-theme-resolved]`, so `auto` shows whichever
 * icon matches the reader's system; `aria-pressed` has to agree with the glyph that is
 * actually painted, which is why it reads the resolved attribute rather than the mode.
 */
function syncThemeButton() {
  const dark = document.documentElement.getAttribute('data-theme-resolved') === 'dark';
  // The setup page carries its own switch, because the top bar is hidden while it runs.
  for (const id of ['theme-toggle', 'setup-theme-toggle']) {
    const button = document.getElementById(id);
    if (button) button.setAttribute('aria-pressed', dark ? 'true' : 'false');
  }
}

/* ---------------------------------------------------------------- view slots */

/**
 * Load one view module on demand and hand it the content root.
 * @param {string} section
 * @param {URLSearchParams} params
 */
async function mountView(section, params) {
  // Every navigation goes through here — the sidebar, a hash link, the boot, the offline
  // retry — so this is the one place the tab title *and* the sidebar's highlight have to
  // be kept in step. The highlight used to be refreshed only by the 30-second health poll,
  // so for half a minute after a click the sidebar pointed at the section you had left
  // while the pane showed the one you asked for.
  setDocumentTitle(section);
  updateNavCurrent(section);
  // Views are loaded on demand and fetched their own data, so an earlier mount can finish
  // *after* a later one. It then cleared the pane and painted itself over the section the
  // operator had actually asked for, which is how the sidebar came to highlight "Users"
  // above a Domains table. Only the newest mount may write to the pane.
  const generation = (mountGeneration += 1);
  const current = () => generation === mountGeneration;
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
      errorState(t('The section “{section}” is not part of this console.', { section }), []),
    );
    return;
  }

  clear(host);
  host.append(loadingState(t('Loading {section}…', { section })));

  try {
    const view = await loader();
    const rendered = await view(params);
    if (!current()) {
      // A newer section took the pane while this one was loading. Its teardown still
      // belongs to whoever mounted it, so it is called rather than dropped.
      try {
        if (rendered.cleanup) rendered.cleanup();
      } catch {
        /* a failing teardown must not block the section that won */
      }
      return;
    }
    clear(host);
    host.append(rendered.node);
    setState({ cleanup: rendered.cleanup || null });
    host.focus({ preventScroll: true });
  } catch (error) {
    if (!current()) return;
    clear(host);
    const message =
      error instanceof ApiError
        ? error.message
        : error instanceof Error
          ? error.message
          : t('This section could not be loaded.');
    host.append(errorState(message));
  }
}

/* ----------------------------------------------------------------- sections */

/**
 * The application name as the document already spells it.
 *
 * `i18n.js` translates `<title>` at load, so this is `Ferroma 管理控制台` on a Chinese
 * console. Every section title is appended to *this*, never to a hardcoded English name.
 */
const BASE_TITLE = document.title;

/**
 * Name the open section in the tab title, so a row of console tabs is readable.
 *
 * The sidebar already answered "where am I" for the person looking at the screen; the
 * tab title answers it for the person looking at ten tabs, and it was the one piece of
 * navigation that never changed. `SECTIONS[].title` — already translated, and until now
 * unused — is what it uses.
 *
 * @param {string} sectionId
 */
function setDocumentTitle(sectionId) {
  const section = SECTIONS.find((entry) => entry.id === sectionId);
  document.title = section ? `${section.title} · ${BASE_TITLE}` : BASE_TITLE;
}

views.dashboard = async () => (await import('./views/dashboard.js')).render;
views.domains = async () => (await import('./views/domains.js')).render;
views.users = async () => (await import('./views/users.js')).render;
views.aliases = async () => (await import('./views/aliases.js')).render;
views.queue = async () => (await import('./views/queue.js')).render;
views.logs = async () => (await import('./views/logs.js')).render;
views.storage = async () => (await import('./views/storage.js')).render;
views.devices = async () => (await import('./views/devices.js')).render;
views.tls = async () => (await import('./views/tls.js')).render;
views.services = async () => (await import('./views/services.js')).render;
views.audit = async () => (await import('./views/audit.js')).render;
views.settings = async () => (await import('./views/settings.js')).render;
views.setup = async () => (await import('./views/setup.js')).render;
// Not a section: the database step exists only while the server has no database, and the
// `bootstrap` route is mounted by `boot()` rather than reached from the sidebar.
views.bootstrap = async () => (await import('./views/bootstrap.js')).render;

/* --------------------------------------------------------------------- boot */

function start() {
  renderNavigation();
  wireChrome();

  setUnauthorizedHandler(() => {
    stopHealthPoll();
    clearTokens();
    setState({ user: null });
    showLogin(t('Your session ended. Sign in again.'));
  });

  onConnectionChange((online) => {
    setHidden(byId('offline-banner'), online);
  });

  onRouteChange((route) => {
    setText(byId('topbar-status'), '');
    // While no administrator exists the only section that can work is the wizard; every
    // other one needs the session that does not exist yet.
    const standalone = bootstrapMode ? 'bootstrap' : setupMode ? 'setup' : null;
    mountView(standalone || route.section, standalone ? new URLSearchParams() : route.params);
  });

  boot().then(async (ok) => {
    if (!ok) return;
    await mountAfterBoot();
  });
}

/** Mount the section the boot landed on: a first-run page, or the routed section. */
async function mountAfterBoot() {
  if (bootstrapMode) {
    // No `go(...)`: the operator's URL is where they were told to look, and rewriting it
    // would only make the reload they are about to do land somewhere else.
    await mountView('bootstrap', new URLSearchParams());
    return;
  }
  if (setupMode) {
    // `replace`, not `push`: the empty hash the operator arrived with should not sit
    // in history in front of the wizard, or Back returns to a console with no session.
    go('setup', {}, { replace: true });
    await mountView('setup', new URLSearchParams());
    return;
  }
  const route = parseHash();
  await mountView(route.section, route.params);
  startHealthPoll();
}

/**
 * Ask the server a question the page cannot proceed without, allowing for the moments when
 * it is not the server answering yet.
 *
 * Both probes below run while the process may still be switching routers — into or out of
 * the bootstrap mode that has no database — and during that window a request can be answered
 * by the front-end's own fallback with HTML (`200`) or fail outright. Read literally, either
 * one means "there is no wizard", which is how a fresh installation was shown a sign-in box
 * for an account that did not exist, with only a hard reload getting past it. A `200` that is
 * not JSON, or a connection that is not there, is therefore retried; any real status is the
 * server's own answer and is taken as such — `404` from `/setup` means the wizard is
 * disabled, which is a decision, not a hiccup.
 */
async function probeWithRetry(path, attempts = 4) {
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    try {
      return await request(path, { toast: false, retryOn401: false });
    } catch (error) {
      const transient = error instanceof ApiError && (error.status === 200 || error.network);
      if (!transient) break;
      if (attempt + 1 < attempts) {
        await new Promise((resolve) => setTimeout(resolve, 300 * (attempt + 1)));
      }
    }
  }
  return null;
}

/**
 * Whether the server has a database yet.
 *
 * The endpoint exists only in that state, so a `404` is the ordinary answer on a configured
 * server and is not an error worth reporting.
 */
async function fetchBootstrapStatus() {
  return probeWithRetry(`${API_BASE}/bootstrap`);
}

/** Whether the server still needs its first administrator, and what it advertises. */
async function fetchSetupStatus() {
  // No wizard, or no API: the ordinary sign-in path is the honest fallback.
  return probeWithRetry(`${API_BASE}/setup`);
}

/** Show the console shell: the wizard on a fresh install, the sections otherwise. */
function enterConsole(options = {}) {
  setupMode = Boolean(options.setup);
  setHidden(byId('login-view'), true);
  setHidden(byId('app-view'), false);
  // Both first-run pages have one screen and no session, so the console's navigation is a
  // single entry that leads nowhere and the account menu has nothing to manage. The shell
  // marks itself and the stylesheet drops the whole chrome.
  byId('app-view').classList.toggle('app-setup', setupMode || bootstrapMode);
  renderNavigation();
  renderAccountMenu();
  renderSidebarMeta();
  viewShown();
}

/** @returns {Promise<boolean>} whether a usable session (or the wizard) is on screen */
async function boot() {
  if (booting) return false;
  booting = true;
  try {
    // Before anything else: is there a database at all? While there is not, the API has
    // only the bootstrap endpoint and a health probe, so every other request would 404 and
    // the console would show a sign-in form for an account that cannot exist yet.
    const bootstrap = await fetchBootstrapStatus();
    if (bootstrap && bootstrap.required) {
      bootstrapMode = true;
      setupMode = false;
      enterConsole({ setup: true });
      return true;
    }

    const [me, health, version] = await Promise.all([
      request(`${API_BASE}/auth/me`, { toast: false, retryOn401: false }),
      request(`${API_BASE}/health`, { toast: false, retryOn401: false }).catch(() => null),
      request(`${API_BASE}/version`, { toast: false, retryOn401: false }).catch(() => null),
    ]);
    setState({ user: me, health, version });
    if (me && me.is_admin === false) {
      showLogin(t('This account is not an administrator.'));
      return false;
    }
    enterConsole();
    return true;
  } catch (error) {
    // No usable session. On a fresh install there is nobody to sign in as, and the wizard
    // that creates the first administrator sits behind this very sign-in panel — so it is
    // unreachable unless the console opens itself for it. `GET /setup` needs no session,
    // which is what makes this decidable.
    const setup = await fetchSetupStatus();
    if (setup && setup.required) {
      enterConsole({ setup: true });
      return true;
    }
    if (error instanceof ApiError && error.network) {
      setHidden(byId('offline-banner'), false);
      showLogin(t('The management API could not be reached.'));
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
  viewShown();
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
  const current = bootstrapMode ? 'bootstrap' : setupMode ? 'setup' : parseHash().section;
  let group = null;

  // Before the first administrator exists every other section would answer `401`, so
  // the sidebar offers the one that works instead of twelve that cannot. Afterwards it
  // offers everything *except* the wizard: an install that has been set up does not need a
  // permanent "Setup" entry that can only answer "an administrator already exists".
  const sections = bootstrapMode
    ? []
    : setupMode
      ? SECTIONS.filter((section) => section.id === 'setup')
      : SECTIONS.filter((section) => section.id !== 'setup');

  for (const section of sections) {
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

function renderAccountMenu() {
  const user = getState().user;
  if (setupMode) {
    setText(byId('account-menu-name'), t('First-run setup'));
    setText(byId('account-menu-email'), '');
    return;
  }
  setText(byId('account-menu-name'), (user && user.display_name) || t('Administrator'));
  setText(byId('account-menu-email'), (user && user.email) || '');
}

function renderSidebarMeta() {
  const { version } = getState();
  const parts = [];
  if (version && version.version) parts.push(`Ferroma ${version.version}`);
  if (version && version.git_sha) parts.push(version.git_sha.slice(0, 7));
  if (version && version.protocol_version !== undefined) parts.push(t('protocol {version}', { version: version.protocol_version }));
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
      setText(byId('login-email-error'), t('Enter your email address.'));
      setHidden(byId('login-email-error'), false);
      byId('login-email').focus();
      return;
    }
    if (password === '') {
      setText(byId('login-password-error'), t('Enter your password.'));
      setHidden(byId('login-password-error'), false);
      byId('login-password').focus();
      return;
    }

    const submit = byId('login-submit');
    submit.disabled = true;
    setText(submit, t('Signing in…'));
    try {
      const payload = await request(`${API_BASE}/auth/login`, {
        method: 'POST',
        body: { email, password },
        toast: false,
        retryOn401: false,
      });
      if (!payload || !payload.user || payload.user.is_admin === false) {
        showLogin(t('That account is not an administrator.'));
        return;
      }
      setTokens(payload);
      byId('login-password').value = '';
      toastSuccess(t('Signed in.'));
      const ok = await boot();
      if (ok) {
        await mountAfterBoot();
      }
    } catch (error) {
      const message =
        error instanceof ApiError && error.status === 401
          ? t('Wrong address or password.')
          : error instanceof ApiError && error.status === 429
            ? error.message
            : error instanceof ApiError && error.network
              ? t('The management API could not be reached.')
              : t('Sign-in failed.');
      showLogin(message);
    } finally {
      submit.disabled = false;
      setText(submit, t('Sign in'));
    }
  });

  // The clicked button is the centre of the reveal; see `shared/theme.js`.
  byId('theme-toggle').addEventListener('click', (event) => toggleTheme({ origin: event.currentTarget }));
  syncThemeButton();
  onThemeChange(syncThemeButton);

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
    toastSuccess(t('Signed out.'));
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
      setText(byId('topbar-status'), t('health unavailable'));
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
  // The server's own token, translated for display: printing it raw left an English
  // `ok` in the corner of a Chinese console.
  const status = health.status ? t(String(health.status)) : t('unknown');
  const failedNote = failed ? ` · ${t('{failed} failed', { failed })}` : '';
  setText(byId('topbar-status'), `${status} · ${t('queue {depth}', { depth })}${failedNote}`);
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
