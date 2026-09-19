/**
 * First-run setup wizard.
 *
 * `GET /api/v1/setup` reports `{ "required": true }` while no admin exists; the
 * section hides itself once an administrator is present, and both endpoints
 * return `409 conflict` after that.
 */

import { API_BASE, ApiError, request, setTokens } from '../../shared/api.js';
import { el, setHidden, setText } from '../../shared/dom.js';
import { t } from '../../shared/i18n.js';
import { go } from '../router.js';
import { toastSuccess } from '../../shared/toast.js';
import { adminCard, badge, field, viewHead } from '../ui.js';

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
  const card = adminCard({
    title: t('Setup'),
    subtitle: 'GET /api/v1/setup',
    actions: [],
    renderData: (data) => data.node,
  });

  const root = el('div', {}, [viewHead(t('First-run setup'), t('Create the first administrator')), card.node]);

  await load();

  async function load() {
    card.setState({ state: 'loading' });
    try {
      const payload = await request(`${API_BASE}/setup`, { toast: false, retryOn401: false });
      const required = Boolean(payload && payload.required);
      if (!required) {
        card.setState({
          state: 'ready',
          data: {
            node: el('div', {}, [
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
          },
        });
        return;
      }
      card.setState({ state: 'ready', data: { node: renderWizard(onDone) } });
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        card.setState({
          state: 'ready',
          data: {
            node: el('div', {}, [
              el('p', {}, [stateBadge('disabled', t('Disabled'))]),
              el('p', {
                class: 'view-sub',
                text: t('The setup wizard is disabled (api.enable_setup_wizard = false) or unimplemented on this build.'),
              }),
            ]),
          },
        });
        return;
      }
      card.setState({
        state: 'error',
        message: error instanceof ApiError ? error.message : t('Setup state could not be read.'),
      });
    }
  }

  async function onDone() {
    toastSuccess(t('Administrator created. Welcome to Ferroma.'));
    await load();
  }

  return { node: root, cleanup() {} };
}

function renderWizard(onDone) {
  const email = el('input', { class: 'input', id: 'setup-email', type: 'email', autocomplete: 'off' });
  const password = el('input', { class: 'input', id: 'setup-password', type: 'password', autocomplete: 'new-password' });
  const hostname = el('input', { class: 'input', id: 'setup-hostname', type: 'text', autocomplete: 'off' });
  const domain = el('input', { class: 'input', id: 'setup-domain', type: 'text', autocomplete: 'off' });
  hostname.value = window.location.hostname || 'localhost';
  const error = el('p', { class: 'field-error', id: 'setup-error', hidden: true });

  const submit = el('button', { type: 'button', class: 'btn btn-primary', text: t('Create administrator') });
  submit.addEventListener('click', async () => {
    const values = {
      email: email.value.trim(),
      password: password.value,
      hostname: hostname.value.trim(),
      domain: domain.value.trim().toLowerCase(),
    };
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(values.email)) {
      show(t('Enter a valid administrator email address.'));
      return;
    }
    if (values.password.length < 8) {
      show(t('The password needs at least 8 characters.'));
      return;
    }
    if (values.hostname === '') {
      show(t('Enter the hostname this server answers on.'));
      return;
    }
    if (!/^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$/.test(values.domain)) {
      show(t('Enter the mail domain to create, for example example.com.'));
      return;
    }

    submit.disabled = true;
    setText(submit, t('Creating…'));
    try {
      const payload = await request(`${API_BASE}/setup`, { method: 'POST', body: values, toast: false });
      if (payload) setTokens(payload);
      onDone();
    } catch (err) {
      if (err instanceof ApiError && err.status === 409) {
        show(t('Setup has already been completed. Reload the console.'));
      } else {
        show(err instanceof ApiError ? err.message : t('Setup failed.'));
      }
    } finally {
      submit.disabled = false;
      setText(submit, t('Create administrator'));
    }
  });

  function show(message) {
    setText(error, message);
    setHidden(error, false);
  }

  return el('div', {}, [
    el('p', {
      class: 'view-sub',
      text: t('No administrator exists yet. This creates the first admin, the domain and its primary address.'),
    }),
    el('div', {}, [
      labelled(t('Administrator email'), email),
      labelled(t('Password'), password),
      labelled(t('Server hostname'), hostname),
      labelled(t('Primary mail domain'), domain),
    ]),
    error,
    el('div', { class: 'card-actions' }, [submit]),
  ]);
}

/** Label + control, using the shared field helper without a table. */
function labelled(label, control) {
  return field(label, control);
}

function linkButton(label, onClick) {
  const node = el('button', { type: 'button', class: 'btn btn-small', text: label });
  node.addEventListener('click', onClick);
  return node;
}
