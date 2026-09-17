/**
 * Small DOM helpers shared by the Webmail modules.
 *
 * Everything here builds nodes with `createElement` / `textContent`. Message
 * bodies are attacker-controlled, so no helper in this file may ever assign
 * `innerHTML`.
 */

/** @param {string} id */
export function byId(id) {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing element #${id}`);
  return el;
}

/**
 * Query a root by CSS selector.
 * @param {string} selector
 * @param {ParentNode} [root]
 */
export function qs(selector, root = document) {
  return root.querySelector(selector);
}

/**
 * @param {string} selector
 * @param {ParentNode} [root]
 */
export function qsa(selector, root = document) {
  return Array.from(root.querySelectorAll(selector));
}

/** Input types whose textual content is their `value`. */
const TEXT_INPUT_TYPES = new Set(['', 'text', 'search', 'email', 'password', 'url', 'tel', 'number']);

/**
 * Create an element.
 * @param {string} tag
 * @param {Record<string, unknown>} [props] `text` sets textContent, `class` the
 *   class list, `dataset` an object of data-* values; everything else becomes an
 *   attribute unless the key names a property.
 * @param {Array<Node|string|null|undefined>} [children]
 */
export function el(tag, props = {}, children = []) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === null || value === undefined || value === false) continue;
    if (key === 'text') {
      node.textContent = String(value);
    } else if (key === 'class') {
      node.className = String(value);
    } else if (key === 'dataset') {
      for (const [k, v] of Object.entries(value)) {
        if (v !== null && v !== undefined) node.dataset[k] = String(v);
      }
    } else if (key === 'value' && (node instanceof HTMLInputElement || node instanceof HTMLTextAreaElement)) {
      node.value = String(value);
    } else {
      node.setAttribute(key, value === true ? '' : String(value));
    }
  }
  for (const child of children) {
    if (child === null || child === undefined) continue;
    node.append(typeof child === 'string' ? document.createTextNode(child) : child);
  }
  return node;
}

/**
 * Write text into a node. On a text-like form control this sets `value`, so the
 * same helper can fill an input and label a paragraph.
 * @param {Element} node
 * @param {string} text
 */
export function setText(node, text) {
  const value = text === null || text === undefined ? '' : String(text);
  if (node instanceof HTMLTextAreaElement) {
    node.value = value;
    return node;
  }
  if (node instanceof HTMLInputElement && TEXT_INPUT_TYPES.has(node.type)) {
    node.value = value;
    return node;
  }
  node.textContent = value;
  return node;
}

/** @param {Element} node @param {boolean} hidden */
export function setHidden(node, hidden) {
  node.hidden = Boolean(hidden);
  return node;
}

/** @param {Node} node */
export function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
  return node;
}

/** @param {Element} node @param {Node|string} child */
export function replace(node, child) {
  clear(node);
  node.append(child);
  return node;
}

/**
 * Truncation-safe label: the visible text stays in the DOM, the full value is
 * exposed through `title` and an aria-label.
 * @param {Element} node
 * @param {string} text
 * @param {string} [labelPrefix]
 */
export function labelWithTitle(node, text, labelPrefix = '') {
  node.textContent = text;
  node.title = text;
  node.setAttribute('aria-label', labelPrefix ? `${labelPrefix}: ${text}` : text);
  return node;
}

/** Inline SVG icon built from a path list; never parses markup from data. */
const ICONS = {
  inbox: ['M3 12h5l2 3h4l2-3h5', 'M3 12 5 5h14l2 7v6a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1v-6Z'],
  sent: ['m4 12 16-8-6 16-2-6-8-2Z'],
  drafts: ['M7 3h7l5 5v13H7z', 'M13 3v6h6'],
  trash: ['M5 7h14M9 7V4h6v3M7 7l1 14h8l1-14'],
  junk: ['M12 3 3 20h18L12 3Z', 'M12 9v4M12 16h.01'],
  archive: ['M3 5h18v4H3z', 'M5 9v11h14V9M9 13h6'],
  folder: ['M3 6h6l2 2h10v11H3z'],
  clip: ['M8.5 12.5 14 7a3 3 0 0 1 4 4l-7.5 7.5a5 5 0 0 1-7-7L11 4'],
  star: ['m12 4 2.4 4.9 5.4.8-3.9 3.8.9 5.4-4.8-2.6-4.8 2.6.9-5.4L4.2 9.7l5.4-.8L12 4Z'],
  doc: ['M7 3h7l5 5v13H7z'],
};

/**
 * @param {keyof typeof ICONS} name
 */
export function svgIcon(name) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  svg.setAttribute('class', 'icon');
  for (const d of ICONS[name] || ICONS.doc) {
    const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    path.setAttribute('d', d);
    path.setAttribute('fill', 'none');
    path.setAttribute('stroke', 'currentColor');
    path.setAttribute('stroke-width', '1.8');
    path.setAttribute('stroke-linecap', 'round');
    path.setAttribute('stroke-linejoin', 'round');
    svg.append(path);
  }
  return svg;
}

/** Focusable descendants, in DOM order. */
const FOCUSABLE = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled]):not([type="hidden"])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[contenteditable="true"]',
  '[tabindex]:not([tabindex="-1"])',
];

/** @param {ParentNode} root */
export function focusable(root) {
  return qsa(FOCUSABLE.join(','), root).filter(
    (node) => node.offsetWidth > 0 || node.offsetHeight > 0 || node === document.activeElement,
  );
}

/**
 * @template {(...args: never[]) => void} F
 * @param {F} fn
 * @param {number} wait
 */
export function debounce(fn, wait) {
  let timer = 0;
  return (...args) => {
    window.clearTimeout(timer);
    timer = window.setTimeout(() => fn(...args), wait);
  };
}
