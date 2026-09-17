/**
 * First-run setup wizard.
 *
 * `GET /api/v1/setup` reports `{ "required": true }` while no admin exists; the
 * section hides itself once an administrator is present, and both endpoints
 * return `409 conflict` after that.
 */

import { API_BASE, ApiError, request, setTokens } from '../api.js';
import { el, setHidden, setText } from '../dom.js';
import { go } from '../router.js';
import { toastSuccess } from '../toast.js';
import { adminCard, badge, field, viewHead } from '../ui.js';

/**
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render() {
  const card = adminCard({
    title: 'Setup',
    subtitle: 'GET /api/v1/setup',
    actions: [],
    renderData: (data) => data.node,
  });

  const root = el('div', {}, [viewHead('First-run setup', 'Create the first administrator'), card.node]);

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
              el('p', {}, [badge('completed'), el('span', { text: ' An administrator already exists.' })]),
              el('p', {
                class: 'view-sub',
                text: 'Both setup endpoints answer 409 conflict from now on. Use Users to add more accounts.',
              }),
              el('div', { class: 'card-actions' }, [linkButton('Go to Users', () => go('users'))]),
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
              el('p', {}, [badge('disabled')]),
              el('p', {
                class: 'view-sub',
                text: 'The setup wizard is disabled (api.enable_setup_wizard = false) or unimplemented on this build.',
              }),
            ]),
          },
        });
        return;
      }
      card.setState({
        state: 'error',
        message: error instanceof ApiError ? error.message : 'Setup state could not be read.',
      });
    }
  }

  async function onDone() {
    toastSuccess('Administrator created. Welcome to Ferroma.');
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

  const submit = el('button', { type: 'button', class: 'btn btn-primary', text: 'Create administrator' });
  submit.addEventListener('click', async () => {
    const values = {
      email: email.value.trim(),
      password: password.value,
      hostname: hostname.value.trim(),
      domain: domain.value.trim().toLowerCase(),
    };
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(values.email)) {
      show('Enter a valid administrator email address.');
      return;
    }
    if (values.password.length < 8) {
      show('The password needs at least 8 characters.');
      return;
    }
    if (values.hostname === '') {
      show('Enter the hostname this server answers on.');
      return;
    }
    if (!/^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$/.test(values.domain)) {
      show('Enter the mail domain to create, for example example.com.');
      return;
    }

    submit.disabled = true;
    setText(submit, 'Creating…');
    try {
      const payload = await request(`${API_BASE}/setup`, { method: 'POST', body: values, toast: false });
      if (payload) setTokens(payload);
      onDone();
    } catch (err) {
      if (err instanceof ApiError && err.status === 409) {
        show('Setup has already been completed. Reload the console.');
      } else {
        show(err instanceof ApiError ? err.message : 'Setup failed.');
      }
    } finally {
      submit.disabled = false;
      setText(submit, 'Create administrator');
    }
  });

  function show(message) {
    setText(error, message);
    setHidden(error, false);
  }

  return el('div', {}, [
    el('p', {
      class: 'view-sub',
      text: 'No administrator exists yet. This creates the first admin, the domain and its primary address.',
    }),
    el('div', {}, [
      labelled('Administrator email', email),
      labelled('Password', password),
      labelled('Server hostname', hostname),
      labelled('Primary mail domain', domain),
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
