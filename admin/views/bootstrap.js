/**
 * The database step: what the console shows when the server has no database yet.
 *
 * The server binds its HTTP port before it can reach PostgreSQL, serves this page, and
 * prints a one-time setup code to its log. Submitting the code and a connection address
 * lets the process finish starting **in place** — no restart — so the page reloads straight
 * into the setup wizard.
 *
 * The code is what keeps the endpoint honest: without it, anyone who can reach the
 * published web port could point this instance at a database of their choosing.
 */

import { API_BASE, ApiError, request } from '../../shared/api.js';
import { el, setHidden, setText } from '../../shared/dom.js';
import { t } from '../../shared/i18n.js';
import { firstRunHeader } from './first-run.js';

/**
 * @returns {Promise<{node: Node, cleanup: () => void}>}
 */
export async function render() {
  const root = el('div', { class: 'setup-view' });
  const state = await load();
  root.append(
    firstRunHeader(
      t('Connect a database'),
      t('Ferroma is running, but it has nowhere to keep mail yet.'),
    ),
  );
  root.append(form(state, root));
  return { node: root, cleanup() {} };
}

/** The bootstrap status, or a synthetic failure the form can show. */
async function load() {
  try {
    return await request(`${API_BASE}/bootstrap`, { toast: false, retryOn401: false });
  } catch (error) {
    return {
      required: true,
      error: error instanceof ApiError ? error.message : t('The server did not describe itself.'),
    };
  }
}

/**
 * @param {Record<string, unknown>} state the `GET /api/v1/bootstrap` body
 * @param {Node} root the view this form lives in, so it can hand the page over on success
 */
function form(state, root) {
  const url = el('input', {
    class: 'input',
    id: 'bootstrap-url',
    type: 'text',
    autocomplete: 'off',
    spellcheck: 'false',
    placeholder: 'postgres://ferroma:password@127.0.0.1:5432/ferroma',
  });
  const code = el('input', {
    class: 'input',
    id: 'bootstrap-code',
    type: 'text',
    autocomplete: 'off',
    spellcheck: 'false',
    autocapitalize: 'characters',
    placeholder: 'ABCD1234',
  });
  const error = el('p', { class: 'field-error', id: 'bootstrap-error', role: 'alert' });
  const previous = typeof state.error === 'string' && state.error !== '' ? state.error : '';
  setText(error, previous);
  setHidden(error, previous === '');

  const submit = el('button', {
    type: 'button',
    class: 'btn btn-primary btn-large',
    text: t('Connect'),
  });

  submit.addEventListener('click', async () => {
    setHidden(error, true);
    const address = url.value.trim();
    if (address === '') {
      setText(error, t('Enter the database address.'));
      setHidden(error, false);
      url.focus();
      return;
    }
    if (code.value.trim() === '') {
      setText(error, t('Enter the setup code from the server log.'));
      setHidden(error, false);
      code.focus();
      return;
    }

    submit.disabled = true;
    setText(submit, t('Connecting…'));
    try {
      await request(`${API_BASE}/bootstrap`, {
        method: 'POST',
        body: { url: address, code: code.value.trim() },
        toast: false,
        retryOn401: false,
      });
      // The server has finished starting on the same port, so the rest of the setup is reachable
      // now — and it belongs on *this* page. Being sent to another address halfway through is
      // exactly what an operator should not have to follow: the database is one of the things
      // they are setting up, not a place they are visiting. The address bar stays at `/`, which
      // becomes the Webmail only once an administrator exists.
      setText(submit, t('Connected. Loading…'));
      await mountWizard(root);
    } catch (err) {
      const message = err instanceof ApiError ? err.message : t('The database could not be reached.');
      setText(error, message);
      setHidden(error, false);
      submit.disabled = false;
      setText(submit, t('Connect'));
    }
  });

  /**
   * Hand this page to the first-run wizard.
   *
   * `POST /bootstrap` returns as soon as the connection is stored; the process then switches
   * from this router to the real API on the same port, and `/api/v1/setup` answers only after
   * that. A probe that lands in that window is retried rather than read as an answer — the
   * wizard's own first request would otherwise be a 404 it reports as "disabled".
   */
  async function mountWizard(view) {
    for (let attempt = 0; attempt < 25; attempt += 1) {
      try {
        await request(`${API_BASE}/setup`, { toast: false, retryOn401: false });
        break;
      } catch (error) {
        const late = error instanceof ApiError && (error.status === 200 || error.status === 0 || error.network);
        if (!late) break;
        if (attempt === 24) {
          // The database is connected either way; a reload lands on whatever is right.
          window.location.reload();
          return;
        }
        await new Promise((resolve) => setTimeout(resolve, 400));
      }
    }
    const { render: renderWizard } = await import('./setup.js');
    const wizard = await renderWizard();
    view.replaceChildren(...wizard.node.childNodes);
  }

  const steps = el('ol', { class: 'bootstrap-steps' }, [
    el('li', { text: t('Create an empty database and a role for Ferroma. It never creates one itself.') }),
    el('li', {
      text: t('Find the setup code in this server\'s log — `docker compose logs ferroma`, or the terminal it runs in. A new code is printed on every start.'),
    }),
    el('li', { text: t('Paste both here. The address is kept in the data directory and reused.') }),
  ]);

  return el('div', { class: 'setup' }, [
    el('div', { class: 'setup-form' }, [
      el('section', { class: 'setup-step' }, [
        el('header', { class: 'setup-step-head' }, [
          el('span', { class: 'setup-step-no', text: '1' }),
          el('div', {}, [
            el('h2', { class: 'setup-step-title', text: t('PostgreSQL') }),
            el('p', {
              class: 'setup-step-lede',
              text: t('Not the Docker one it might have started next to: Ferroma connects over the network, so the address is whatever this process can reach.'),
            }),
          ]),
        ]),
        el('div', { class: 'setup-step-body' }, [
          el('div', { class: 'field' }, [el('label', { class: 'field-label', for: 'bootstrap-url', text: t('Database address') }), url]),
          el('div', { class: 'field' }, [el('label', { class: 'field-label', for: 'bootstrap-code', text: t('Setup code') }), code]),
          steps,
          error,
          el('div', { class: 'setup-actions' }, [submit]),
        ]),
      ]),
    ]),
  ]);
}
