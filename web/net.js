/**
 * Network helpers that both apps and the API client share.
 */

/**
 * Combine a caller signal with a wall-clock timeout.
 * @param {number} ms
 * @param {AbortSignal} [signal]
 * @returns {AbortSignal|undefined}
 */
export function timeoutSignal(ms, signal) {
  if (typeof AbortSignal === 'undefined' || typeof AbortSignal.timeout !== 'function') {
    return signal;
  }
  const timer = AbortSignal.timeout(ms);
  if (!signal) return timer;
  if (typeof AbortSignal.any === 'function') return AbortSignal.any([signal, timer]);
  return signal;
}
