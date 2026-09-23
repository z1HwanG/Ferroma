#!/usr/bin/env node
/** Focused policy checks; run with `node web/tools/security-regressions.mjs`. */
import assert from 'node:assert/strict';
import { framePolicy, hasRemoteImages } from '../reader.js';

const blocked = framePolicy(false);
const allowed = framePolicy(true);
assert.match(blocked, /http-equiv="Content-Security-Policy"/);
assert.match(blocked, /default-src 'none'/);
assert.match(blocked, /img-src data:;/);
assert.match(blocked, /style-src 'unsafe-inline'/);
assert.doesNotMatch(blocked, /img-src[^;]*https?:/);
assert.match(allowed, /img-src data: http: https:;/);
assert.match(allowed, /default-src 'none'/);
assert.match(allowed, /style-src 'unsafe-inline'/);
for (const html of [
  '<img src=https://tracker.test/open>',
  '<img src="//tracker.test/open">',
  '<img srcset="https://tracker.test/a 1x">',
  '<div style="background:url(https://tracker.test/open)">',
]) assert.equal(hasRemoteImages(html), true, html);
assert.equal(hasRemoteImages('<img src="cid:logo"><a href="https://example.test">link</a>'), false);
console.log('PASS — reader remote-content policy');
