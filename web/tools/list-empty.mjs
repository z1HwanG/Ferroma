#!/usr/bin/env node
/**
 * An empty list must not keep the previous folder's "n / total" pill.
 *
 * `renderList` used to write `#list-count` only on the path that had rows, then
 * return. Switching to a folder whose page came back empty left "3 / 3" beside
 * "This folder is empty."
 */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const nodes = new Map();
function node(id) {
  let current = nodes.get(id);
  if (!current) {
    current = {
      id,
      hidden: false,
      textContent: '',
      children: [],
      attributes: {},
      classList: { toggle() {}, remove() {} },
      dataset: {},
      setAttribute(name, value) { this.attributes[name] = value; },
      append(...kids) { this.children.push(...kids); },
      querySelectorAll() { return []; },
    };
    nodes.set(id, current);
  }
  return current;
}

const state = {
  folder: { id: 9 },
  mailboxId: 1,
  search: '',
  messages: [{ id: 1, seen: false, flagged: false, hasAttachments: false, subject: 'old' }],
  total: 3,
  selectedId: 0,
  checked: new Set(),
  listLoading: false,
  hasMore: false,
  lastCheckedIndex: -1,
};

const mock = {
  document: {
    activeElement: null,
    getElementById: () => null,
    createDocumentFragment: () => ({ append() {} }),
  },
  Element: function Element() {},
  getState: () => state,
  t: (text) => text,
  byId: (id) => node(id),
  setText: (target, text) => { target.textContent = text == null ? '' : String(text); },
  setHidden: (target, hidden) => { target.hidden = Boolean(hidden); },
  clear: (target) => { target.children = []; },
  el: (tag, attrs = {}) => ({
    tag,
    attrs,
    children: [],
    dataset: {},
    append() {},
    setAttribute() {},
    addEventListener() {},
  }),
  labelWithTitle: () => {},
  svgIcon: () => ({ setAttribute() {} }),
  timeElement: () => ({}),
};

const source = readFileSync(new URL('../list.js', import.meta.url), 'utf8')
  .replace(/^import .*;\s*$/gm, '')
  .replace(/^export /gm, '');
vm.runInNewContext(`${source}\nglobalThis.renderListForTest = renderList;`, mock);

mock.renderListForTest();
assert.equal(node('list-count').textContent, '1 / 3', 'a loaded row still shows the pill');

state.messages = [];
state.total = 0;
mock.renderListForTest();
assert.equal(node('list-empty').hidden, false);
assert.equal(node('list-empty').textContent, 'This folder is empty.');
assert.equal(
  node('list-count').textContent,
  '',
  'an empty folder must not keep the previous "n / total"',
);

console.log('PASS — empty list clears the count pill');
