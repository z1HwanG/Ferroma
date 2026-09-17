/**
 * Admin console state: the signed-in operator, the boot-time health snapshot the
 * sidebar reuses, the queue-depth history the dashboard sparkline draws, and the
 * mounted view's teardown hook.
 *
 * Views read and write it directly — there is no observer layer, because every
 * view repaints itself after its own request resolves.
 */

const state = {
  user: null,
  version: null,
  health: null,
  /** @type {null | (() => void)} teardown for the mounted view */
  cleanup: null,
  /** @type {Array<{t: number, depth: number, failed: number}>} */
  queueHistory: [],
};

export function getState() {
  return state;
}

/**
 * @param {Partial<typeof state>} patch
 */
export function setState(patch) {
  Object.assign(state, patch);
}
