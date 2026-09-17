/**
 * Ferroma management-API client.
 *
 * Responsibilities:
 *  * attach the bearer token (or rely on the `ferroma_session` cookie),
 *  * parse the documented `{ "error": { "code", "message", "details" } }` envelope,
 *  * refresh the access token once on `401` and retry the original request,
 *  * report network failures so the shell can show its offline banner,
 *  * surface failures as toasts, except when a caller opts out.
 *
 * This file is duplicated verbatim into `web/` and `admin/` on purpose: the server
 * serves each directory independently, so a shared parent path would not be
 * reachable from either app.
 */

import { toastError } from './toast.js';
import { timeoutSignal } from './net.js';

export const API_BASE = '/api/v1';
export const REQUEST_TIMEOUT_MS = 20000;

const ACCESS_KEY = 'ferroma.access_token';
const REFRESH_KEY = 'ferroma.refresh_token';

/* ------------------------------------------------------------------ storage */

function readStored(key) {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeStored(key, value) {
  try {
    if (value) window.localStorage.setItem(key, value);
    else window.localStorage.removeItem(key);
  } catch {
    /* storage unavailable: the request still carries whatever is in memory */
  }
}

/* -------------------------------------------------------------------- token */

let accessToken = readStored(ACCESS_KEY);
let refreshToken = readStored(REFRESH_KEY);
let onUnauthorized = null;
let refreshing = null;
const connectionListeners = new Set();
let online = true;

export function setTokens(tokens) {
  if (tokens && typeof tokens.access_token === 'string') {
    accessToken = tokens.access_token;
    writeStored(ACCESS_KEY, accessToken);
  }
  if (tokens && 'refresh_token' in tokens) {
    refreshToken = tokens.refresh_token || null;
    writeStored(REFRESH_KEY, refreshToken);
  }
}

export function clearTokens() {
  accessToken = null;
  refreshToken = null;
  writeStored(ACCESS_KEY, null);
  writeStored(REFRESH_KEY, null);
}

export function hasToken() {
  return Boolean(accessToken);
}

/** Install the callback invoked when refreshing is impossible. */
export function setUnauthorizedHandler(handler) {
  onUnauthorized = handler;
}

/* --------------------------------------------------------------- connection */

/** @param {(online: boolean) => void} listener */
export function onConnectionChange(listener) {
  connectionListeners.add(listener);
  return () => connectionListeners.delete(listener);
}

function setOnline(next) {
  if (online === next) return;
  online = next;
  for (const listener of connectionListeners) listener(next);
}

export function isOnline() {
  return online;
}

/* ------------------------------------------------------------------- errors */

export class ApiError extends Error {
  /**
   * @param {string} message
   * @param {{status?: number, code?: string, details?: unknown, network?: boolean}} [info]
   */
  constructor(message, info = {}) {
    super(message);
    this.name = 'ApiError';
    this.status = info.status ?? 0;
    this.code = info.code ?? (info.network ? 'network_error' : 'internal_error');
    this.details = info.details;
    this.network = Boolean(info.network);
  }
}

function envelopeError(status, payload) {
  const envelope = payload && typeof payload === 'object' ? payload.error : null;
  if (envelope && typeof envelope === 'object') {
    return new ApiError(
      typeof envelope.message === 'string' ? envelope.message : `Request failed (HTTP ${status})`,
      { status, code: envelope.code, details: envelope.details },
    );
  }
  return new ApiError(`Request failed (HTTP ${status})`, { status });
}

/* ----------------------------------------------------------------- requests */

/**
 * @param {string} path path starting with `/api/v1`
 * @param {{method?: string, body?: unknown, headers?: Record<string,string>, signal?: AbortSignal,
 *          retryOn401?: boolean, toast?: boolean, raw?: boolean, timeoutMs?: number}} [options]
 */
export async function request(path, options = {}) {
  if (!path.startsWith(API_BASE)) {
    throw new ApiError(`internal: ${path} is not an ${API_BASE} path`, { code: 'internal_error' });
  }

  const headers = Object.assign({ Accept: 'application/json' }, options.headers || {});
  if (accessToken) headers.Authorization = `Bearer ${accessToken}`;

  /** @type {RequestInit} */
  const init = {
    method: options.method || 'GET',
    headers,
    credentials: 'same-origin',
    signal: timeoutSignal(options.timeoutMs || REQUEST_TIMEOUT_MS, options.signal),
  };

  if (options.body instanceof FormData) {
    init.body = options.body;
  } else if (options.body !== undefined) {
    headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(options.body);
  }

  let response;
  try {
    response = await fetch(new URL(path, window.location.origin).toString(), init);
  } catch (error) {
    if (error && error.name === 'AbortError') throw error;
    setOnline(false);
    const failure = new ApiError('The server could not be reached.', { network: true });
    if (options.toast !== false) toastError(failure.message);
    throw failure;
  }

  setOnline(true);

  if (response.status === 401 && options.retryOn401 !== false) {
    const refreshed = await tryRefresh(options.signal);
    if (refreshed) return request(path, Object.assign({}, options, { retryOn401: false }));
    if (onUnauthorized) onUnauthorized();
    throw new ApiError('Your session has expired. Sign in again.', { status: 401, code: 'unauthorized' });
  }

  if (response.status === 204) return null;

  if (!response.ok) {
    let payload = null;
    try {
      payload = await response.json();
    } catch {
      payload = null;
    }
    const failure = envelopeError(response.status, payload);
    if (response.status === 429) {
      const retryAfter = Number(response.headers.get('Retry-After'));
      if (Number.isFinite(retryAfter) && retryAfter > 0) {
        failure.message = `${failure.message} Try again in ${retryAfter}s.`;
      }
    }
    if (options.toast !== false) toastError(failure.message);
    throw failure;
  }

  if (options.raw) return response;
  if (response.status === 205) return null;
  const text = await response.text();
  if (text === '') return null;
  try {
    return JSON.parse(text);
  } catch {
    throw new ApiError('The server returned a response that is not JSON.', { status: response.status });
  }
}

/** Refresh the access token once; concurrent callers share a single attempt. */
async function tryRefresh(signal) {
  if (!refreshToken) return false;
  if (refreshing) return refreshing;
  refreshing = (async () => {
    try {
      const response = await fetch(new URL(`${API_BASE}/auth/refresh`, window.location.origin).toString(), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ refresh_token: refreshToken }),
        signal,
      });
      if (!response.ok) return false;
      const payload = await response.json();
      setTokens(payload);
      return Boolean(payload && payload.access_token);
    } catch {
      return false;
    } finally {
      refreshing = null;
    }
  })();
  return refreshing;
}

/** Build a query string, dropping empty values. */
export function query(params) {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params || {})) {
    if (value === undefined || value === null || value === '') continue;
    search.set(key, String(value));
  }
  const text = search.toString();
  return text === '' ? '' : `?${text}`;
}

/**
 * Download an attachment (or the raw message) through the same credentials, then
 * hand the bytes to the browser as a file.
 * @param {string} path path starting with `/api/v1`
 * @param {string} filename
 */
export async function download(path, filename) {
  const response = await request(path, { raw: true });
  const blob = await response.blob();
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement('a');
  anchor.href = url;
  anchor.download = filename || 'download';
  anchor.rel = 'noopener';
  document.body.append(anchor);
  anchor.click();
  anchor.remove();
  window.setTimeout(() => URL.revokeObjectURL(url), 30000);
}
