/**
 * Localisation for both front-ends.
 *
 * # The English text is the key
 *
 * `t('Sign out')` is looked up as the literal string `Sign out`. This is a deliberate
 * choice, not a shortcut. The alternative — a semantic key such as `nav.signOut` — needs
 * one invented identifier per string, and there are around eight hundred of them across
 * the two apps. With the English text as the key, wrapping a string is mechanical, a
 * missing translation degrades to readable English instead of to a bare key, and a
 * checker can list exactly which strings the catalog still owes.
 *
 * The cost is that rewording an English string orphans its translation. That is visible
 * rather than silent: the coverage rule in `tools/check.mjs` fails on a catalog entry
 * nothing uses and on a `t()` key the catalog does not cover.
 *
 * # Interpolation and plurals
 *
 * `t('Delete {count} folders?', { count })` substitutes `{name}` placeholders.
 * `tn(count, '{count} message', '{count} messages', { count })` picks the English
 * singular or plural form and translates that; both forms are separate catalog entries,
 * because a language may need the same translation for both (Chinese does) or a third
 * form English does not have.
 */

import { zhCN } from './locales/zh-CN.js';

/**
 * The languages this build ships, in the order the picker shows them, each labelled in
 * its own language so a reader who cannot read the current one can still find theirs.
 */
export const LOCALES = [
  { tag: 'en', label: 'English' },
  { tag: 'zh-CN', label: '简体中文' },
];

/** The locale used when nothing else applies. English is the source language. */
export const DEFAULT_LOCALE = 'en';

/**
 * The catalogs, keyed by locale tag. English is absent on purpose: its catalog is the
 * identity function, which is what makes an untranslated string readable.
 */
const CATALOGS = { 'zh-CN': zhCN };

const STORAGE_KEY = 'ferroma.locale';

/** @type {Set<(tag: string) => void>} */
const listeners = new Set();

/** @type {string} */
let locale = initialLocale();

/**
 * The stored choice, else English.
 *
 * The browser's language is deliberately *not* consulted: this instance is usually reached
 * by several people, and a UI that changes language because one of them re-installed their
 * laptop is a UI nobody can give instructions for. English is the default and the picker is
 * one click away, on the sign-in card and in Settings.
 */
function initialLocale() {
  return readStored() || DEFAULT_LOCALE;
}

/**
 * Map a BCP 47 tag onto a shipped locale.
 *
 * Every Chinese region selects Simplified: this build ships one Chinese catalog, and
 * answering a Traditional reader in Simplified is closer than answering in English.
 * @param {unknown} tag
 * @returns {string|null}
 */
export function normaliseTag(tag) {
  if (typeof tag !== 'string') return null;
  const primary = tag.trim().toLowerCase().split(/[-_]/)[0];
  if (primary === 'zh') return 'zh-CN';
  if (primary === 'en') return 'en';
  return null;
}

/** The locale in force. */
export function currentLocale() {
  return locale;
}

/**
 * Switch locale and tell everyone who asked to know.
 *
 * Nothing re-renders itself: the two apps are plain ES modules that build their DOM
 * once, so a view that has already drawn would keep its old text. The language picker
 * reloads the page after calling this, which is the one way to guarantee that every
 * string — including the ones inside a dialog that is currently open — is redrawn.
 * @param {string} tag
 */
export function setLocale(tag) {
  const next = normaliseTag(tag) || DEFAULT_LOCALE;
  if (next === locale) return;
  locale = next;
  writeStored(next);
  applyDocumentLanguage();
  for (const listener of listeners) listener(next);
}

/**
 * @param {(tag: string) => void} listener
 * @returns {() => void} the unsubscribe
 */
export function onLocaleChange(listener) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Keep `<html lang>` honest, so a screen reader and the browser's font picker agree. */
export function applyDocumentLanguage() {
  if (typeof document !== 'undefined') document.documentElement.lang = locale;
}

/**
 * Translate one string.
 *
 * @param {string} text the English source text, used as the key
 * @param {Record<string, unknown>} [params] values for `{name}` placeholders
 * @returns {string} the translation, the English text when there is none
 */
export function t(text, params) {
  if (typeof text !== 'string') return '';
  const catalog = CATALOGS[locale];
  const translated = catalog && Object.prototype.hasOwnProperty.call(catalog, text)
    ? catalog[text]
    : text;
  return interpolate(translated, params);
}

/**
 * Translate the singular or plural English form, whichever fits.
 *
 * @param {number} count
 * @param {string} one the English singular, e.g. `'{count} message'`
 * @param {string} other the English plural
 * @param {Record<string, unknown>} [params]
 */
export function tn(count, one, other, params) {
  return t(count === 1 ? one : other, params);
}

/**
 * Translate the static text of the app shell.
 *
 * Both `index.html` files carry their English text inline, so the page is readable
 * before a single script runs. An element that should be translated is marked — the
 * marker's absence means "leave this alone", which is what keeps brand names, counts
 * and addresses that happen to sit in the shell out of the catalog:
 *
 * | Marker | What it translates |
 * |---|---|
 * | `data-i18n` | the element's text, which is used as the key |
 * | `data-i18n-placeholder` | its `placeholder` |
 * | `data-i18n-title` | its `title` |
 * | `data-i18n-aria-label` | its `aria-label` |
 *
 * Marking every translatable node is what makes the shell checkable: rule 11 of
 * `tools/check.mjs` holds both directions — a marker whose text is not in the catalog
 * fails, and so does a static text node that *is* in the catalog but carries no marker.
 * Without the markers the second half is undecidable and a new shell string could ship
 * untranslated with every check green.
 *
 * It is idempotent — a translated node no longer matches a key — and it touches text
 * and attribute values only, never structure.
 *
 * It runs once at module load, before either `main.js` body executes and after the
 * document has been parsed (both apps load their entry at the end of `<body>`); the
 * views that build their own DOM afterwards go through `t()` themselves.
 *
 * @param {ParentNode} [root]
 */
export function translateDocument(root) {
  if (typeof document === 'undefined') return;
  // `documentElement`, not `body`: `<title>` lives in `<head>`, and a scope of `body`
  // left the browser tab and the bookmarks naming the app in English.
  const scope = root || document.documentElement;
  if (!scope) return;
  for (const node of scope.querySelectorAll('[data-i18n]')) {
    const key = (node.textContent || '').trim();
    if (key !== '') node.textContent = t(key);
  }
  for (const [marker, attribute] of [
    ['data-i18n-placeholder', 'placeholder'],
    ['data-i18n-title', 'title'],
    ['data-i18n-aria-label', 'aria-label'],
  ]) {
    for (const node of scope.querySelectorAll(`[${marker}]`)) {
      const value = node.getAttribute(attribute);
      if (value !== null) node.setAttribute(attribute, t(value));
    }
  }
}

/**
 * Substitute `{name}` placeholders. An unknown placeholder is left in place rather
 * than replaced with `undefined`: the reader then sees which value is missing.
 * @param {string} text
 * @param {Record<string, unknown>|undefined} params
 */
function interpolate(text, params) {
  if (!params) return text;
  return text.replace(/\{(\w+)\}/g, (match, name) =>
    Object.prototype.hasOwnProperty.call(params, name) ? String(params[name]) : match);
}

/** The catalogs, for the coverage rule in `tools/check.mjs`. */
export function catalogs() {
  return CATALOGS;
}

/** The shipped locale tags. */
export function shippedLocales() {
  return LOCALES.map((entry) => entry.tag);
}

/* ------------------------------------------------------------------- storage */

function readStored() {
  try {
    const stored = window.localStorage.getItem(STORAGE_KEY);
    return stored ? normaliseTag(stored) : null;
  } catch {
    return null;
  }
}

function writeStored(tag) {
  try {
    window.localStorage.setItem(STORAGE_KEY, tag);
  } catch {
    /* storage unavailable: the choice lasts for this page only */
  }
}

// Both are done once at load, before any view draws: the locale lands on `<html lang>`,
// and the shell's inline English is replaced where the catalog has a translation.
applyDocumentLanguage();
translateDocument();
