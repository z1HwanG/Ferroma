/**
 * Email address parsing and validation, mirroring the rules the API enforces:
 * a non-empty local part, an `@`, and a domain with labels that may not start or
 * end with a hyphen and that carries a TLD of at least two characters.
 */

import { el, focusable } from './dom.js';

const LOCAL_RE = /^[A-Za-z0-9!#$%&'*+/=?^_`{|}~.-]+$/;
const DOMAIN_RE = /^(?=.{1,253}$)(?!-)[A-Za-z0-9-]{1,63}(?<!-)(?:\.(?!-)[A-Za-z0-9-]{1,63}(?<!-))*\.[A-Za-z]{2,63}$/;

/**
 * Validate one address.
 * @param {string} value
 * @returns {{ok: true, address: string} | {ok: false, reason: string}}
 */
export function validateAddress(value) {
  const address = String(value || '').trim();
  if (address === '') return { ok: false, reason: 'address is empty' };
  const at = address.lastIndexOf('@');
  if (at <= 0) return { ok: false, reason: 'an address needs a local part and an @domain' };
  if (address.indexOf('@') !== at) return { ok: false, reason: 'an address may contain only one @' };
  const local = address.slice(0, at);
  const domain = address.slice(at + 1);
  if (local.length > 64) return { ok: false, reason: 'the local part is longer than 64 characters' };
  if (local.startsWith('.') || local.endsWith('.') || local.includes('..')) {
    return { ok: false, reason: 'the local part may not start, end or double a dot' };
  }
  if (!LOCAL_RE.test(local)) return { ok: false, reason: 'the local part contains invalid characters' };
  if (domain === '') return { ok: false, reason: 'the domain is missing' };
  if (!DOMAIN_RE.test(domain)) return { ok: false, reason: `“${domain}” is not a valid domain` };
  return { ok: true, address };
}

/**
 * Split a raw header value on commas and semicolons.
 * @param {string} raw
 */
function splitAddresses(raw) {
  return String(raw || '')
    .split(/[,;\n]/)
    .map((part) => part.trim())
    .filter((part) => part !== '');
}

/**
 * A chip input: type an address, press Enter/comma/Tab to commit it.
 *
 * @param {{id: string, label: string, placeholder?: string, onInvalid?: (message: string) => void}} options
 */
export function createChipField(options) {
  const input = el('input', {
    type: 'text',
    id: options.id,
    autocomplete: 'off',
    spellcheck: 'false',
    placeholder: options.placeholder || '',
  });
  input.setAttribute('aria-labelledby', `${options.id}-label`);
  input.setAttribute('aria-describedby', `${options.id}-error`);

  const chips = el('div', { class: 'chip-field' }, [input]);
  const error = el('p', { class: 'field-error', id: `${options.id}-error`, hidden: true });
  const wrapper = el('div', { class: 'field' }, [
    el('span', { class: 'field-label', id: `${options.id}-label`, text: options.label }),
    chips,
    error,
  ]);

  /** @type {string[]} */
  const values = [];

  const report = (message) => {
    if (options.onInvalid) options.onInvalid(message);
  };

  const showError = (message) => {
    error.textContent = message;
    error.hidden = false;
    chips.classList.add('is-invalid');
  };

  const clearError = () => {
    error.hidden = true;
    error.textContent = '';
    chips.classList.remove('is-invalid');
  };

  const renderChips = () => {
    for (const button of Array.from(chips.querySelectorAll('.chip'))) button.remove();
    for (const value of values) {
      const remove = el('button', {
        type: 'button',
        class: 'chip-remove',
        'aria-label': `Remove ${value}`,
        text: '\u00d7',
      });
      remove.addEventListener('click', () => {
        values.splice(values.indexOf(value), 1);
        renderChips();
        input.focus();
      });
      const chip = el('span', { class: 'chip' }, [
        el('span', { class: 'chip-label', title: value, text: value }),
        remove,
      ]);
      chips.insertBefore(chip, input);
    }
  };

  /** @param {string} value */
  const commit = (value) => {
    const trimmed = value.trim();
    if (trimmed === '') return true;
    const result = validateAddress(trimmed);
    if (!result.ok) {
      showError(`“${trimmed}” is not a valid address — ${result.reason}.`);
      report(error.textContent);
      return false;
    }
    if (!values.includes(result.address)) values.push(result.address);
    clearError();
    renderChips();
    return true;
  };

  const commitPending = () => {
    if (input.value.trim() === '') return true;
    const ok = commit(input.value);
    if (ok) input.value = '';
    return ok;
  };

  input.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' || event.key === ',') {
      if (input.value.trim() === '') return;
      event.preventDefault();
      if (commit(input.value)) input.value = '';
      return;
    }
    if (event.key === 'Tab' && !event.shiftKey && input.value.trim() !== '') {
      // Commit, then keep Tab's direction: shift focus by hand inside a dialog
      // (whose focus trap would otherwise pull it back), and let the browser
      // move it everywhere else.
      commit(input.value);
      input.value = '';
      const dialog = input.closest('[role="dialog"]');
      if (dialog) {
        event.preventDefault();
        const order = focusable(dialog);
        const next = order[order.indexOf(input) + 1];
        if (next) next.focus();
      }
    } else if (event.key === 'Backspace' && input.value === '' && values.length) {
      values.pop();
      renderChips();
    }
  });

  input.addEventListener('blur', () => {
    commitPending();
  });

  return {
    root: wrapper,
    input,
    clearError,
    getValues: () => values.slice(),
    setValues(next) {
      values.length = 0;
      for (const candidate of next) {
        const result = validateAddress(candidate);
        if (result.ok && !values.includes(result.address)) values.push(result.address);
      }
      renderChips();
      clearError();
    },
    commitPending,
    focus: () => input.focus(),
  };
}
