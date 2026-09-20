/**
 * First-run setup wizard.
 *
 * `GET /api/v1/setup` reports `{ "required": true }` while no admin exists; the
 * section hides itself once an administrator is present, and both endpoints
 * return `409 conflict` after that.
 *
 * The wizard is the *installation's* configuration surface, not a shortcut for one
 * command: everything an operator would otherwise pass as a flag or an environment
 * variable — the domain, the hostname, the public URL, the API listener, the TLS files —
 * is a field here, stored in the database and adopted by the server on its next start
 * (see `apply_stored_settings`). Three things cannot move here, and the page says so
 * rather than pretending otherwise: the database connection (a process has to reach
 * PostgreSQL before it can serve a page, and the settings table lives in it), the
 * container image, and the published ports (Docker fixes port mappings when the container
 * is created).
 */

import { API_BASE, ApiError, clearTokens, request, setTokens } from '../../shared/api.js';
import { el, setHidden, setText } from '../../shared/dom.js';
import { t } from '../../shared/i18n.js';
import { consoleUrl, go } from '../router.js';
import { toastSuccess } from '../../shared/toast.js';
import { firstRunHeader } from './first-run.js';
import { badge, errorState, field, isValidDomain } from '../ui.js';

/**
 * The state pill, showing a translated label.
 *
 * The raw value still decides the pill's colour; only the text is replaced.
 *
 * @param {unknown} state
 * @param {string} label an already-translated label
 * @returns {Element}
 */
function stateBadge(state, label) {
  const node = badge(state);
  node.textContent = label;
  return node;
}

/**
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render() {
  const root = el('div', { class: 'setup-view' });

  try {
    const payload = await request(`${API_BASE}/setup`, { toast: false, retryOn401: false });
    if (payload && payload.required) {
      root.append(renderWizard(payload));
    } else {
      root.append(
        el('div', { class: 'setup-done' }, [
          el('p', {}, [
            stateBadge('completed', t('Completed')),
            el('span', { text: ` ${t('An administrator already exists.')}` }),
          ]),
          el('p', {
            class: 'view-sub',
            text: t('Both setup endpoints answer 409 conflict from now on. Use Users to add more accounts.'),
          }),
          el('div', { class: 'card-actions' }, [linkButton(t('Go to Users'), () => go('users'))]),
        ]),
      );
    }
  } catch (error) {
    if (error instanceof ApiError && error.status === 404) {
      root.append(
        el('div', { class: 'setup-done' }, [
          el('p', {}, [stateBadge('disabled', t('Disabled'))]),
          el('p', {
            class: 'view-sub',
            text: t('The setup wizard is disabled (api.enable_setup_wizard = false) or unimplemented on this build.'),
          }),
        ]),
      );
    } else {
      root.append(errorState(error instanceof ApiError ? error.message : t('Setup state could not be read.')));
    }
  }

  return { node: root, cleanup() {} };
}

/**
 * The wizard form.
 *
 * @param {Record<string, unknown>} status the `GET /setup` body the form prefills from
 */
function renderWizard(status) {
  const email = el('input', {
    class: 'input',
    id: 'setup-email',
    type: 'email',
    autocomplete: 'off',
    placeholder: 'admin@example.com',
  });
  const password = el('input', {
    class: 'input',
    id: 'setup-password',
    type: 'password',
    autocomplete: 'new-password',
  });
  const domain = el('input', {
    class: 'input',
    id: 'setup-domain',
    type: 'text',
    autocomplete: 'off',
    placeholder: 'example.com',
  });
  const description = el('input', {
    class: 'input',
    id: 'setup-domain-description',
    type: 'text',
    autocomplete: 'off',
  });
  const hostname = el('input', { class: 'input', id: 'setup-hostname', type: 'text', autocomplete: 'off' });
  const publicUrl = el('input', { class: 'input', id: 'setup-public-url', type: 'url', autocomplete: 'off' });
  const apiHost = el('input', { class: 'input', id: 'setup-api-host', type: 'text', autocomplete: 'off' });
  const apiPort = el('input', { class: 'input', id: 'setup-api-port', type: 'number', min: '1', max: '65535' });
  const tlsEnabled = el('input', { type: 'checkbox', id: 'setup-tls-enabled' });
  const tlsCert = el('input', { class: 'input', id: 'setup-tls-cert', type: 'text', autocomplete: 'off' });
  const tlsKey = el('input', { class: 'input', id: 'setup-tls-key', type: 'text', autocomplete: 'off' });

  const meta = status || {};
  hostname.value = String(meta.hostname || window.location.hostname || 'localhost');
  publicUrl.value = String(meta.public_url || window.location.origin || '');
  apiHost.value = String(meta.api_host || '0.0.0.0');
  apiPort.value = String(meta.api_port || 8080);
  tlsEnabled.checked = Boolean(meta.tls_enabled);
  tlsCert.value = String(meta.tls_cert || '');
  tlsKey.value = String(meta.tls_key || '');

  const tlsFields = el('div', { class: 'setup-tls-fields' }, [
    field(t('Certificate bundle (leaf first)'), tlsCert, t('A PEM file, as the process sees it — inside a container that is the mounted path.')),
    field(t('Private key'), tlsKey),
  ]);
  const syncTls = () => setHidden(tlsFields, !tlsEnabled.checked);
  tlsEnabled.addEventListener('change', syncTls);
  syncTls();

  const error = el('p', { class: 'field-error', id: 'setup-error', hidden: true });
  const notice = el('div', { class: 'setup-notice', hidden: true });

  // The administrator's address has to live inside the domain being created, so the email
  // field keeps offering that domain as its placeholder while it is being typed.
  const syncPlaceholder = () => {
    email.placeholder = `admin@${domain.value.trim().toLowerCase()}`;
  };
  domain.addEventListener('input', syncPlaceholder);
  syncPlaceholder();

  const submit = el('button', { type: 'button', class: 'btn btn-primary btn-large', text: t('Create administrator') });
  // Once the wizard has succeeded the account exists; a second click can only answer
  // "setup already completed", which reads like a failure. The button therefore stays
  // disabled, and the restart notice carries its own way forward.
  let finished = false;
  submit.addEventListener('click', async () => {
    if (finished) return;
    const values = {
      email: email.value.trim(),
      password: password.value,
      domain: domain.value.trim().toLowerCase(),
      domain_description: description.value.trim(),
      hostname: hostname.value.trim(),
      public_url: publicUrl.value.trim(),
      api_host: apiHost.value.trim(),
      api_port: Number.parseInt(apiPort.value, 10),
      tls_enabled: tlsEnabled.checked,
      tls_cert: tlsCert.value.trim(),
      tls_key: tlsKey.value.trim(),
    };
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(values.email)) {
      show(t('Enter a valid administrator email address.'));
      return;
    }
    if (values.password.length < 8) {
      show(t('The password needs at least 8 characters.'));
      return;
    }
    if (!isValidDomain(values.domain)) {
      show(t('Enter the mail domain to create, for example example.com.'));
      return;
    }
    if (!values.email.toLowerCase().endsWith(`@${values.domain}`)) {
      show(t('The administrator address must be inside {domain}.', { domain: values.domain }));
      return;
    }
    if (values.hostname === '' || !isValidDomain(values.hostname)) {
      show(t('Enter the hostname this server answers on.'));
      return;
    }
    if (values.public_url !== '' && !/^https?:\/\//.test(values.public_url)) {
      show(t('The public URL must start with http:// or https://.'));
      return;
    }
    if (values.api_host === '') {
      show(t('Enter the address the API should listen on.'));
      return;
    }
    if (!Number.isInteger(values.api_port) || values.api_port < 1 || values.api_port > 65535) {
      show(t('The API port must be between 1 and 65535.'));
      return;
    }
    if (values.tls_enabled && (values.tls_cert === '' || values.tls_key === '')) {
      show(t('TLS needs both a certificate and its key.'));
      return;
    }

    submit.disabled = true;
    setText(submit, t('Creating…'));
    try {
      const payload = await request(`${API_BASE}/setup`, { method: 'POST', body: values, toast: false });
      if (payload) setTokens(payload);
      const applied = payload && payload.applied ? payload.applied : null;
      finished = true;
      if (applied && applied.restart_required) {
        // The session exists and the administrator is usable; the settings above are read when
        // the server starts, so it is restarting itself right now and this page waits for it.
        showRestartNotice(applied);
        return;
      }
      toastSuccess(t('Administrator created. Welcome to Ferroma.'));
      // Nothing needed a restart, so the server is already the one this instance will be: the
      // handoff is the same, minus the waiting.
      if (!(await landOnSignIn())) window.location.replace('/');
    } catch (err) {
      if (err instanceof ApiError && err.status === 409) {
        // Someone else finished the wizard between the page load and this click. The
        // account exists, so there is nothing left to submit.
        finished = true;
        show(t('Setup has already been completed. Reload the console.'));
      } else {
        show(err instanceof ApiError ? err.message : t('Setup failed.'));
      }
    } finally {
      submit.disabled = finished;
      setText(submit, t('Create administrator'));
    }
  });

  function showRestartNotice(applied) {
    const rows = [
      applied.hostname ? [t('Hostname'), applied.hostname] : null,
      applied.public_url ? [t('Public URL'), applied.public_url] : null,
      applied.api_host ? [t('Listen address'), applied.api_host] : null,
      applied.api_port ? [t('Listen port'), String(applied.api_port)] : null,
      applied.tls_enabled === undefined ? null : [t('TLS'), applied.tls_enabled ? t('on') : t('off')],
      applied.tls_cert ? [t('Certificate bundle (leaf first)'), applied.tls_cert] : null,
      applied.tls_key ? [t('Private key'), applied.tls_key] : null,
    ].filter(Boolean);

    // The settings above are read once, when the server starts — which is why it comes back up
    // by itself rather than asking the operator to go and restart a container: a wizard that
    // ends with "now restart this" looks like it did nothing, and the step it just took is the
    // one thing the person who filled it in cannot check.
    const status = el('p', { text: t('The server is restarting to apply these settings…') });
    const manual = el('p', {
      text: t('The administrator was created. These settings are stored and take effect after the next restart of Ferroma (or its container):'),
    });

    notice.replaceChildren(
      status,
      el('ul', { class: 'setup-summary' }, rows.map(([label, value]) => summaryItem(label, value))),
      manual,
      el('div', { class: 'card-actions' }, [
        linkButton(t('Continue to the console'), () => window.location.replace(consoleUrl())),
      ]),
    );
    setHidden(manual, true);
    setHidden(notice, false);
    setHidden(error, true);
    toastSuccess(t('Administrator created.'));
    waitForRestart(status, manual);
  }

  /**
   * Wait for the process to answer again, then walk into the console.
   *
   * The server replaced itself a moment ago (see `restart_itself` in the server binary), so the
   * page cannot navigate until it listens again. Nothing to do but wait — and to say so, rather
   * than spin forever, if it never comes back.
   *
   * The probe goes through `request()` like every other call, so `shared/api.js` stays the only
   * file that talks to the network. A server that answers *at all* is a server that is back:
   * `/health` answers `503` with the health document while there is no database, and that
   * arrives here as an `ApiError` carrying the status.
   */
  async function waitForRestart(status, manual) {
    if (await landOnSignIn()) return;
    setText(status, t('The server did not come back.'));
    setHidden(manual, false);
  }

  /**
   * Wait for the instance to be *serving* again, then hand the operator the sign-in page.
   *
   * Two probes with a pause between them, because the first successful answer can still come
   * from the process that is on its way out — and landing in that gap is exactly what left a
   * blank shell on the screen after a fresh install. The health endpoint and the static files
   * are the same router, so an answer that survives the pause means the page will load.
   *
   * The tokens the wizard minted are dropped on the way: a fresh install is a handoff, not a
   * session. The operator just chose a password, and the only way to see that it works is to use
   * it — `/` is the Webmail's sign-in card, and the console takes the same credentials at
   * `/admin/`.
   *
   * @returns {Promise<boolean>} whether the server came back
   */
  async function landOnSignIn() {
    for (let attempt = 0; attempt < 40; attempt += 1) {
      if (!(await answers())) {
        await new Promise((resolve) => setTimeout(resolve, 500));
        continue;
      }
      await new Promise((resolve) => setTimeout(resolve, 1500));
      if (!(await answers())) {
        await new Promise((resolve) => setTimeout(resolve, 500));
        continue;
      }
      clearTokens();
      window.location.replace('/');
      return true;
    }
    clearTokens();
    return false;
  }

  /** Whether the API is answering at all — its status does not matter, its presence does. */
  async function answers() {
    try {
      await request(`${API_BASE}/health`, { toast: false, retryOn401: false });
      return true;
    } catch (error) {
      // A real HTTP status means a running server; anything else is the gap between two of them.
      return error instanceof ApiError && error.status >= 400;
    }
  }

  function show(message) {
    setText(error, message);
    setHidden(error, false);
  }

  return el('div', { class: 'setup' }, [
    firstRunHeader(
      t('Welcome to Ferroma'),
      t('Three steps to get started. The rest lives in the console.'),
    ),

    el('div', { class: 'setup-form' }, [
      setupStep(1, t('Administrator account'), t('The account that signs in here, and the first mailbox.'), [
        el('div', { class: 'row' }, [
          field(t('Administrator email'), email),
          field(t('Password'), password, t('At least 8 characters.')),
        ]),
      ]),
      setupStep(2, t('Mail domain'), t('The domain this server receives mail for. Addresses, DKIM and the DNS checks are all grouped by it.'), [
        el('div', { class: 'row' }, [
          field(t('Primary mail domain'), domain),
          field(t('Description'), description, t('Optional, shown in the domain list.')),
        ]),
      ]),
      setupStep(3, t('Server and access'), t('Stored in the database and applied on the next restart. A value given by the deployment — a flag or an environment variable — always wins over these.'), [
        el('div', { class: 'row' }, [
          field(t('Server hostname'), hostname, t('The name this server announces in SMTP and message headers.')),
          field(t('Public URL'), publicUrl, t('Where this server is reached, e.g. https://mail.example.com. Used in autoconfiguration and generated links.')),
        ]),
        el('div', { class: 'row' }, [
          field(t('Listen address'), apiHost, t('0.0.0.0 reaches the API from outside the container; 127.0.0.1 keeps it on the loopback behind a proxy.')),
          field(t('Listen port'), apiPort),
        ]),
        el('label', { class: 'checkbox', for: 'setup-tls-enabled' }, [
          tlsEnabled,
          el('span', { text: t('Serve TLS from this process') }),
        ]),
        tlsFields,
      ]),
      error,
      notice,
      el('div', { class: 'setup-actions' }, [submit]),
    ]),
  ]);
}

/** One numbered step of the form. */
function setupStep(number, title, lede, children) {
  return el('section', { class: 'setup-step' }, [
    el('header', { class: 'setup-step-head' }, [
      el('span', { class: 'setup-step-no', text: String(number) }),
      el('div', {}, [
        el('h2', { class: 'setup-step-title', text: title }),
        el('p', { class: 'setup-step-lede', text: lede }),
      ]),
    ]),
    el('div', { class: 'setup-step-body' }, children),
  ]);
}

/** One `label — value` line of the summary or the restart notice. */
function summaryItem(label, value) {
  return el('li', {}, [
    el('span', { class: 'setup-summary-label', text: label }),
    el('span', { class: 'setup-summary-value', text: value }),
  ]);
}

function linkButton(label, onClick) {
  const node = el('button', { type: 'button', class: 'btn btn-small', text: label });
  node.addEventListener('click', onClick);
  return node;
}
