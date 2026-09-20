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

/**
 * Inline SVG icons, built from a path list; never parses markup from data.
 *
 * Every icon is a stroked 24×24 drawing with the same 1.8 weight and round joins, so a
 * tray glyph beside a gear beside an arrow reads as one family rather than three. The
 * few solid shapes a mail interface needs (a filled star, an unread dot) are listed in
 * [`FILLED`] instead of being stroked.
 */
const ICONS = {
  inbox: ['M3 12h5l2 3h4l2-3h5', 'M3 12 5 5h14l2 7v6a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1v-6Z'],
  sent: ['m4 12 16-8-6 16-2-6-8-2Z'],
  drafts: ['M7 3h7l5 5v13H7z', 'M13 3v6h6'],
  trash: ['M5 7h14M9 7V4h6v3M7 7l1 14h8l1-14'],
  junk: ['M12 3 3 20h18L12 3Z', 'M12 9v4M12 16h.01'],
  archive: ['M3 5h18v4H3z', 'M5 9v11h14V9M9 13h6'],
  folder: ['M3 6h6l2 2h10v11H3z'],
  folderOpen: ['M3 6h6l2 2h9v3', 'M3 6v13h18l-2-9H8l-2 3H3'],
  clip: ['M8.5 12.5 14 7a3 3 0 0 1 4 4l-7.5 7.5a5 5 0 0 1-7-7L11 4'],
  star: ['m12 4 2.4 4.9 5.4.8-3.9 3.8.9 5.4-4.8-2.6-4.8 2.6.9-5.4L4.2 9.7l5.4-.8L12 4Z'],
  starFilled: ['m12 4 2.4 4.9 5.4.8-3.9 3.8.9 5.4-4.8-2.6-4.8 2.6.9-5.4L4.2 9.7l5.4-.8L12 4Z'],
  dot: ['M12 10.5a1.5 1.5 0 1 0 0 3 1.5 1.5 0 0 0 0-3Z'],
  doc: ['M7 3h7l5 5v13H7z'],
  reply: ['M9 14 4 9l5-5', 'M4 9h10a5 5 0 0 1 5 5v3'],
  replyAll: ['M8 14 3 9l5-5', 'M13 14 8 9l5-5', 'M8 9h7a5 5 0 0 1 5 5v3'],
  forward: ['m15 14 5-5-5-5', 'M20 9H10a5 5 0 0 0-5 5v3'],
  mail: ['M3 6h18v12H3z', 'm3 7 9 6 9-6'],
  move: ['M3 6h6l2 2h10v11H3z', 'M12 10v6M9.5 13.5 12 16l2.5-2.5'],
  code: ['m8 8-4 4 4 4', 'm16 8 4 4-4 4', 'm13.5 5-3 14'],
  refresh: ['M20 11a8 8 0 1 0-1.2 5.2', 'M20 5v6h-6'],
  search: ['M11 17a6 6 0 1 0 0-12 6 6 0 0 0 0 12Z', 'M15.5 15.5 21 21'],
  menu: ['M3 6h18M3 12h18M3 18h18'],
  close: ['M6 6l12 12M18 6 6 18'],
  check: ['m5 12.5 4.5 4.5L19 7'],
  plus: ['M12 5v14M5 12h14'],
  pencil: ['M4 20h4l10-10-4-4L4 16v4Z', 'm14 6 4 4'],
  chevronDown: ['m6 9 6 6 6-6'],
  chevron: ['m9 6 6 6-6 6'],
  sun: ['M12 16a4 4 0 1 0 0-8 4 4 0 0 0 0 8Z', 'M12 2v2M12 20v2M2 12h2M20 12h2M5 5l1.4 1.4M17.6 17.6 19 19M19 5l-1.4 1.4M6.4 17.6 5 19'],
  moon: ['M20 14.5A8.5 8.5 0 0 1 9.5 4a8.5 8.5 0 1 0 10.5 10.5Z'],
  contrast: ['M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18Z', 'M12 3v18'],
  // Sliders rather than a gear: at 16–18px a ring with eight spokes reads as a sun,
  // and this icon sits beside a theme toggle in both front-ends.
  settings: [
    'M4 7h3M11 7h9',
    'M4 12h9M17 12h3',
    'M4 17h2M10 17h10',
    'M9 5a2 2 0 1 0 0 4 2 2 0 0 0 0-4Z',
    'M15 10a2 2 0 1 0 0 4 2 2 0 0 0 0-4Z',
    'M8 15a2 2 0 1 0 0 4 2 2 0 0 0 0-4Z',
  ],
  user: ['M12 11a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7Z', 'M4.5 20c0-3.4 3.4-5.4 7.5-5.4s7.5 2 7.5 5.4'],
  warning: ['M12 4 3 20h18L12 4Z', 'M12 10v4M12 17h.01'],
  info: ['M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18Z', 'M12 11v5M12 8h.01'],
  clock: ['M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18Z', 'M12 7v5l3 2'],
  download: ['M12 4v10', 'm8 11 4 4 4-4', 'M5 19h14'],
};

/** The icons drawn solid rather than stroked. */
const FILLED = new Set(['starFilled', 'dot']);

/**
 * @param {keyof typeof ICONS} name
 */
export function svgIcon(name) {
  const filled = FILLED.has(name);
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  svg.setAttribute('class', 'icon');
  for (const d of ICONS[name] || ICONS.doc) {
    const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    path.setAttribute('d', d);
    if (filled) {
      path.setAttribute('fill', 'currentColor');
    } else {
      path.setAttribute('fill', 'none');
      path.setAttribute('stroke', 'currentColor');
      path.setAttribute('stroke-width', '1.8');
      path.setAttribute('stroke-linecap', 'round');
      path.setAttribute('stroke-linejoin', 'round');
    }
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
