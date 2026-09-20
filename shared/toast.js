/**
 * Toasts. Success and information go to a polite live region, failures to an
 * assertive one, so a screen reader announces errors without stealing focus.
 */

import { byId, el } from './dom.js';
import { t } from './i18n.js';

const MAX_VISIBLE = 4;

/**
 * @param {'success'|'error'|'info'} kind
 * @param {string} message
 * @param {{timeout?: number}} [options]
 */
export function toast(kind, message, options = {}) {
  const region = kind === 'error' ? byId('alert-region') : byId('toast-region');
  const text = message === null || message === undefined || message === '' ? t('Something went wrong.') : String(message);

  const close = () => {
    node.remove();
  };

  const closeButton = el('button', {
    type: 'button',
    class: 'toast-close',
    'aria-label': t('Dismiss notification'),
    text: '\u00d7',
  });
  closeButton.addEventListener('click', close);

  const node = el('div', { class: `toast toast-${kind}` }, [
    el('p', { class: 'toast-text', text }),
    closeButton,
  ]);

  region.append(node);
  while (region.children.length > MAX_VISIBLE) region.firstElementChild.remove();

  const lifetime = options.timeout ?? (kind === 'error' ? 9000 : 5000);
  // The stylesheet drains a bar over exactly this long, so a toast on its way out says so
  // instead of disappearing from under the reader.
  node.style.setProperty('--toast-life', `${lifetime}ms`);
  window.setTimeout(close, lifetime);
}

export const toastSuccess = (message, options) => toast('success', message, options);
export const toastError = (message, options) => toast('error', message, options);
export const toastInfo = (message, options) => toast('info', message, options);
