/**
 * Modal dialogs.
 *
 * Every dialog: traps Tab, closes on Escape, closes on a click outside the card,
 * and restores focus to whatever was focused before it opened. The card itself
 * carries `role="dialog"` and `aria-modal="true"`.
 */

import { byId, el, focusable } from './dom.js';

/** @type {HTMLElement[]} */
const stack = [];

function topDialog() {
  return stack.length ? stack[stack.length - 1] : null;
}

function onKeydown(event) {
  const current = topDialog();
  if (!current) return;
  if (event.key === 'Escape') {
    event.preventDefault();
    current.close('escape');
    return;
  }
  if (event.key !== 'Tab') return;
  const candidates = focusable(current.card);
  if (candidates.length === 0) {
    event.preventDefault();
    current.card.focus();
    return;
  }
  const first = candidates[0];
  const last = candidates[candidates.length - 1];
  const active = document.activeElement;
  if (event.shiftKey && (active === first || !current.card.contains(active))) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && (active === last || !current.card.contains(active))) {
    event.preventDefault();
    first.focus();
  }
}

/**
 * @typedef {object} ModalOptions
 * @property {string} title
 * @property {Node} body
 * @property {Node[]} [footer]
 * @property {string} [hostId] which host container to mount into
 * @property {string} [size] `wide` for the larger card
 * @property {boolean} [closeOnOutside]
 * @property {(reason: string) => boolean|void} [onClose] return `false` to veto
 * @property {(reason: string) => boolean} [beforeClose] return `false` to keep the
 *   dialog open (used for "discard this draft?" confirmations)
 * @property {(card: HTMLElement, api: {close: (reason?: string) => void}) => void} [onMount]
 */

/**
 * @param {ModalOptions} options
 */
export function openModal(options) {
  const host = byId(options.hostId || 'modal-host');
  const restoreTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;

  const card = el('div', {
    class: `modal-card${options.size === 'wide' ? ' modal-wide' : ''}`,
    role: 'dialog',
    'aria-modal': 'true',
    'aria-label': options.title,
    tabindex: '-1',
  });

  const titleId = `modal-title-${stack.length + 1}-${Date.now()}`;
  card.setAttribute('aria-labelledby', titleId);

  const title = el('h2', { class: 'modal-title', id: titleId, text: options.title });
  const closeButton = el('button', {
    type: 'button',
    class: 'btn btn-icon',
    'aria-label': 'Close dialog',
    text: '\u00d7',
  });

  const head = el('div', { class: 'modal-head' }, [title, closeButton]);
  const body = el('div', { class: 'modal-body' }, [options.body]);
  card.append(head, body);

  if (options.footer && options.footer.length) {
    card.append(el('div', { class: 'modal-foot' }, options.footer));
  }

  const overlay = el('div', { class: 'modal' }, [card]);

  const entry = {
    card,
    overlay,
    close(reason = 'programmatic') {
      if (!stack.includes(entry)) return;
      if (options.beforeClose && options.beforeClose(reason) === false) return;
      const verdict = options.onClose ? options.onClose(reason) : true;
      if (verdict === false) return;
      stack.splice(stack.indexOf(entry), 1);
      overlay.remove();
      if (stack.length === 0) {
        document.removeEventListener('keydown', onKeydown, true);
        document.body.classList.remove('no-scroll');
      }
      const next = topDialog();
      if (next) next.card.focus();
      else if (restoreTo && document.contains(restoreTo)) restoreTo.focus();
    },
  };

  closeButton.addEventListener('click', () => entry.close('close-button'));
  overlay.addEventListener('mousedown', (event) => {
    if (event.target === overlay && options.closeOnOutside !== false) entry.close('outside');
  });

  stack.push(entry);
  document.body.classList.add('no-scroll');
  document.addEventListener('keydown', onKeydown, true);
  host.append(overlay);

  if (options.onMount) options.onMount(card, entry);

  const first = focusable(card)[0];
  (first || card).focus();

  return entry;
}

/**
 * A destructive-action confirmation. Resolves `true` when confirmed.
 * @param {{title: string, message: string, confirmLabel?: string, dangerous?: boolean}} options
 * @returns {Promise<boolean>}
 */
export function confirmDialog(options) {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (value) => {
      if (settled) return;
      settled = true;
      resolve(value);
    };

    const cancel = el('button', { type: 'button', class: 'btn', text: 'Cancel' });
    const confirm = el('button', {
      type: 'button',
      class: `btn ${options.dangerous === false ? 'btn-primary' : 'btn-danger'}`,
      text: options.confirmLabel || 'Confirm',
    });
    const message = el('p', { class: 'modal-message', text: options.message });
    const body = el('div', {}, [message]);

    const modal = openModal({
      title: options.title,
      body,
      footer: [cancel, confirm],
      hostId: 'dialog-host',
      onClose: () => {
        finish(false);
      },
      onMount: (card) => {
        cancel.addEventListener('click', () => modal.close('cancel'));
        confirm.addEventListener('click', () => {
          finish(true);
          modal.close('confirm');
        });
        card.addEventListener('keydown', (event) => {
          if (event.key === 'Enter' && document.activeElement !== cancel) {
            event.preventDefault();
            finish(true);
            modal.close('confirm');
          }
        });
      },
    });

    confirm.focus();
  });
}

/**
 * A single-line prompt with an optional exact-match confirmation string.
 * @param {{title: string, label: string, confirmLabel?: string, requireValue?: string, hint?: string}} options
 * @returns {Promise<string|null>}
 */
export function promptDialog(options) {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (value) => {
      if (settled) return;
      settled = true;
      resolve(value);
    };

    const inputId = `prompt-input-${Date.now()}`;
    const input = el('input', { class: 'input', id: inputId, type: 'text', autocomplete: 'off' });
    const error = el('p', { class: 'field-error', hidden: true });
    const children = [
      el('label', { class: 'field-label', for: inputId, text: options.label }),
      input,
    ];
    if (options.hint) children.push(el('p', { class: 'modal-message', text: options.hint }));
    children.push(error);
    const body = el('div', {}, children);

    const cancel = el('button', { type: 'button', class: 'btn', text: 'Cancel' });
    const confirm = el('button', {
      type: 'button',
      class: 'btn btn-danger',
      text: options.confirmLabel || 'Confirm',
    });

    const submit = () => {
      const value = input.value.trim();
      if (options.requireValue !== undefined && value !== options.requireValue) {
        error.textContent = `Type “${options.requireValue}” exactly to continue.`;
        error.hidden = false;
        input.focus();
        return;
      }
      if (value === '') {
        error.textContent = 'This value is required.';
        error.hidden = false;
        input.focus();
        return;
      }
      finish(value);
      modal.close('confirm');
    };

    const modal = openModal({
      title: options.title,
      body,
      footer: [cancel, confirm],
      hostId: 'dialog-host',
      onClose: () => finish(null),
      onMount: (card) => {
        cancel.addEventListener('click', () => modal.close('cancel'));
        confirm.addEventListener('click', submit);
        card.addEventListener('keydown', (event) => {
          if (event.key === 'Enter') {
            event.preventDefault();
            submit();
          }
        });
      },
    });

    input.focus();
  });
}
