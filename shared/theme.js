/**
 * Theme handling.
 *
 * The default follows the system: a desktop that has been dark all day means the interface
 * should be too, and the first visit is the one case where the system's own setting is the best
 * evidence about the person in front of the screen. An explicit choice — the toggle in the top
 * bar, or the picker in Settings, which offers light, dark and "match system" — is remembered
 * and wins from then on.
 *
 * This reverses the behaviour up to 0.1.4, which defaulted to light on the argument that a UI
 * turning black because somebody's laptop is set up that way is unexpected. That argument is
 * about a decision nobody made; a first visit is not a decision, it is the absence of one.
 */

const STORAGE_KEY = 'ferroma.theme';
const MODES = ['auto', 'light', 'dark'];

const listeners = new Set();
let mode = 'auto';

function safeRead() {
  try {
    return window.localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

function safeWrite(value) {
  try {
    window.localStorage.setItem(STORAGE_KEY, value);
  } catch {
    /* storage unavailable (private mode); the in-memory mode still applies */
  }
}

/** Apply the current mode to <html>. */
function apply() {
  const root = document.documentElement;
  root.setAttribute('data-theme', mode);
  const dark =
    mode === 'dark' ||
    (mode === 'auto' && window.matchMedia('(prefers-color-scheme: dark)').matches);
  root.style.colorScheme = dark ? 'dark' : 'light';
  // The toggle's icon keys off the *resolved* scheme, not the mode: `auto` can be dark
  // on one machine and light on the next, and a toggle showing a sun while the screen is
  // dark is worse than no icon at all.
  root.setAttribute('data-theme-resolved', dark ? 'dark' : 'light');
}

/** How long every surface is allowed to cross-fade after a theme change. */
const SWITCH_MS = 420;
/** How long the circular reveal takes when the browser can animate one. */
const REVEAL_MS = 620;
let switchTimer = 0;
let haloTimer = 0;

/** Reduced motion turns every one of these effects off, in one place. */
function motionAllowed() {
  return !window.matchMedia('(prefers-reduced-motion: reduce)').matches;
}

/**
 * The point the reveal grows from: the toggle that was asked, or the corner it lives in.
 *
 * The effect is only convincing when it starts where the click happened — a circle that
 * blooms out of the middle of the screen reads as a page transition, not as "this button
 * changed the light".
 *
 * @param {Element|null} origin
 */
function revealOrigin(origin) {
  const node = toggleNode(origin);
  if (node) {
    const box = node.getBoundingClientRect();
    return { x: box.left + box.width / 2, y: box.top + box.height / 2 };
  }
  return { x: window.innerWidth - 56, y: 40 };
}

/**
 * The button the effect belongs to.
 *
 * The caller passes the element it acted on — the webmail has two of them, the top bar's
 * and the sign-in card's, and this module has no business knowing either one's id. When
 * nobody passed one (a system scheme change), it falls back to the app's own toggle.
 *
 * @param {unknown} origin
 * @returns {Element|null}
 */
function toggleNode(origin) {
  if (origin instanceof Element) return origin;
  return document.getElementById('theme-toggle');
}

/** Ring the toggle once, so the switch has a point of origin as well as a direction. */
function pulseToggle(origin) {
  const node = toggleNode(origin);
  if (!node || !motionAllowed()) return;
  node.classList.remove('theme-pulsing');
  // Reading `offsetWidth` restarts the animation when the same button is clicked twice.
  void node.offsetWidth;
  node.classList.add('theme-pulsing');
  if (haloTimer) window.clearTimeout(haloTimer);
  haloTimer = window.setTimeout(() => node.classList.remove('theme-pulsing'), REVEAL_MS);
}

/**
 * Whether the browser can wipe the new scheme in from a point.
 *
 * `document.startViewTransition` snapshots the page before and after one DOM update and
 * lets CSS animate between the two. It is the difference between a cross-fade, which reads
 * as "the colours changed", and an expanding circle, which reads as "I turned the light
 * on" — and it is what the whole effect below is built on.
 */
function canReveal() {
  return typeof document.startViewTransition === 'function' && motionAllowed();
}

/** Tell the listeners the mode changed. */
function announce() {
  for (const listener of listeners) listener(mode);
}

/**
 * Open a brief window in which every surface transitions its colours.
 *
 * The transition cannot live permanently on every element: a `transition` of all
 * colours on every node makes ordinary hover states sluggish and shows up in profiles.
 * It is added for the length of one switch instead — see `.theme-switching` in each
 * app's stylesheet — and skipped entirely when the reader asked for reduced motion.
 */
function flashSwitch() {
  const root = document.documentElement;
  if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) return;
  root.classList.add('theme-switching');
  if (switchTimer) window.clearTimeout(switchTimer);
  switchTimer = window.setTimeout(() => root.classList.remove('theme-switching'), SWITCH_MS);
}

/** Read the persisted mode without touching the DOM. */
export function currentTheme() {
  return mode;
}

/**
 * The scheme actually on screen: `auto` resolved against the system.
 *
 * The difference matters to a *toggle*: `currentTheme()` returns the mode, which is `auto`
 * until someone chooses, so flipping from it turned "auto" into an explicit `dark` — a
 * click that visibly did nothing whenever the system was already dark.
 *
 * @returns {'light'|'dark'}
 */
export function resolvedTheme() {
  return document.documentElement.getAttribute('data-theme-resolved') === 'dark' ? 'dark' : 'light';
}

/**
 * Switch to the other scheme, whatever the mode was.
 *
 * @param {{origin?: Element|null}} [options] the control that asked, for the reveal's centre
 */
export function toggleTheme(options = {}) {
  setTheme(resolvedTheme() === 'dark' ? 'light' : 'dark', options);
}

/**
 * Switch the theme.
 *
 * The change itself is instant in the DOM; what the reader sees is either the circular
 * reveal (where the browser supports one) or the older cross-fade. Both are skipped when
 * the interface is asked to reduce motion, in which case the switch is simply instant.
 *
 * @param {string} next one of `auto`, `light`, `dark`
 * @param {{origin?: Element|null}} [options] the control that asked, for the reveal's centre
 */
export function setTheme(next, options = {}) {
  const previous = mode;
  mode = MODES.includes(next) ? next : 'auto';
  safeWrite(mode);
  const changed = mode !== previous;

  if (changed && canReveal()) {
    const { x, y } = revealOrigin(options.origin);
    const radius = Math.hypot(
      Math.max(x, window.innerWidth - x),
      Math.max(y, window.innerHeight - y),
    );
    pulseToggle(options.origin);
    try {
      const transition = document.startViewTransition(() => {
        apply();
        announce();
      });
      transition.ready
        .then(() => {
          document.documentElement.animate(
            { clipPath: [`circle(0px at ${x}px ${y}px)`, `circle(${radius}px at ${x}px ${y}px)`] },
            {
              duration: REVEAL_MS,
              easing: 'cubic-bezier(0.22, 0.61, 0.36, 1)',
              pseudoElement: '::view-transition-new(root)',
            },
          );
        })
        .catch(() => {
          /* the transition was skipped (a second click, or a hidden tab): the theme is
             already applied, so there is nothing to repair */
        });
      return;
    } catch {
      /* fall through to the cross-fade below */
    }
  }

  if (changed) {
    flashSwitch();
    pulseToggle(options.origin);
  }
  apply();
  announce();
}

/** @param {(mode: string) => void} listener */
export function onThemeChange(listener) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Called once at boot, before the first paint of the app. */
export function initTheme() {
  const stored = safeRead();
  mode = MODES.includes(stored) ? stored : 'auto';
  apply();
  const media = window.matchMedia('(prefers-color-scheme: dark)');
  const onChange = () => {
    if (mode !== 'auto') return;
    // The system flipping its own scheme is a theme change like any other: it gets the
    // same reveal (or cross-fade), and the listeners hear about it so a toggle can
    // restate itself. There is no clicked button here, so the origin is the toggle's own
    // corner — see `revealOrigin`.
    if (canReveal()) {
      const { x, y } = revealOrigin(null);
      const radius = Math.hypot(
        Math.max(x, window.innerWidth - x),
        Math.max(y, window.innerHeight - y),
      );
      const transition = document.startViewTransition(() => {
        apply();
        announce();
      });
      transition.ready
        .then(() => {
          document.documentElement.animate(
            { clipPath: [`circle(0px at ${x}px ${y}px)`, `circle(${radius}px at ${x}px ${y}px)`] },
            {
              duration: REVEAL_MS,
              easing: 'cubic-bezier(0.22, 0.61, 0.36, 1)',
              pseudoElement: '::view-transition-new(root)',
            },
          );
        })
        .catch(() => {});
      return;
    }
    flashSwitch();
    apply();
    announce();
  };
  if (typeof media.addEventListener === 'function') media.addEventListener('change', onChange);
  else if (typeof media.addListener === 'function') media.addListener(onChange);
}

/** The theme list the settings dialog offers. */
export const THEME_MODES = MODES.slice();
