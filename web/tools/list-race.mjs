#!/usr/bin/env node
/** Simulate out-of-order list HTTP responses without a browser/server. */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const pending = [];
const state = {
  folder: { id: 1 }, mailboxId: 1, search: '', messages: [], total: 0,
  listLoading: false, hasMore: false,
};
const mock = {
  API_BASE: '/api',
  getState: () => state,
  getPrefs: () => ({ perPage: 25 }),
  mutate: (change) => change(state),
  query: (params) => JSON.stringify(params),
  request: (url) => new Promise((resolve, reject) => pending.push({ url, resolve, reject })),
  messagesOf: (payload) => payload.messages,
  totalOf: (payload) => payload.total,
  byId: () => ({ hidden: false }),
  setHidden: () => {}, setText: () => {},
  ApiError: class ApiError extends Error {},
};
const source = readFileSync(new URL('../list.js', import.meta.url), 'utf8')
  .replace(/^import .*;\s*$/gm, '')
  .replace(/^export /gm, '');
vm.runInNewContext(`${source}\nglobalThis.loadMessagesForTest = loadMessages;`, mock);
const load = mock.loadMessagesForTest;

const first = load({ reset: true });
assert.equal(pending.length, 1);
state.folder = { id: 2 };
state.messages = [];
const second = load({ reset: true });
assert.equal(pending.length, 2, 'new folder may load while prior request is pending');
pending[1].resolve({ messages: [{ id: 2 }], total: 1 });
await second;
pending[0].resolve({ messages: [{ id: 1 }], total: 1 });
await first;
assert.deepEqual(state.messages.map((message) => message.id), [2]);
assert.equal(state.listLoading, false);

const third = load({ reset: true });
state.search = 'new search';
state.messages = [];
const fourth = load({ reset: true });
pending[2].reject(new Error('stale network failure'));
await third;
assert.equal(state.listLoading, true, 'stale failure must not clear the new loading flag');
pending[3].resolve({ messages: [{ id: 4 }], total: 1 });
await fourth;
assert.deepEqual(state.messages.map((message) => message.id), [4]);
console.log('PASS — list folder/search race');
