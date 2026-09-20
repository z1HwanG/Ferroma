/**
 * A blank-page guard for both front-ends.
 *
 * It is loaded *before* each app's `main.js` and imports nothing, so it survives the one
 * failure it exists for: a module that never loads. Both `index.html` files start with
 * every view hidden, so an app whose entry module 404s, whose static import fails, or
 * whose boot never settles leaves the operator a white rectangle that is
 * indistinguishable from a server that never answered. This reveals a panel naming what
 * went wrong instead.
 *
 * Its own text is English. It runs before the language catalog is fetched, and a
 * diagnostic that needs a working build to be readable is not a diagnostic.
 *
 * The interface is deliberately two functions and no dependencies: nothing here may
 * throw, because it is the last thing between a misconfigured deployment and a blank
 * page.
 */
(function () {
  'use strict';

  /** How long to wait for a view before saying so. */
  var WATCHDOG_MS = 15000;
  var settled = false;

  function describe(detail) {
    var text = 'The Ferroma interface did not start.';
    if (detail) text += ' ' + detail;
    return (
      text +
      ' Reload the page; if it stays blank, check the browser console and that the ' +
      '/shared modules and this app\u2019s own scripts are being served.'
    );
  }

  /**
   * Show the failure. Does nothing when a view is already on screen: an error inside a
   * running console must not tear down the console around it.
   *
   * It writes into the sign-in panel's own error line, which both apps already have, and
   * falls back to appending a bare paragraph. Nothing here may depend on markup that a
   * future edit could rename.
   */
  function reveal(detail) {
    try {
      var app = document.getElementById('app-view');
      if (app && !app.hidden) return;
      var login = document.getElementById('login-view');
      if (app) app.hidden = true;
      if (login) login.hidden = false;
      if (!login) return;

      var target = document.getElementById('login-error');
      if (!target) {
        target = document.createElement('p');
        target.className = 'field-error';
        target.setAttribute('role', 'alert');
        login.appendChild(target);
      }
      target.textContent = describe(detail);
      target.hidden = false;
    } catch (error) {
      /* The document is beyond repair; there is nothing else to try. */
    }
  }

  // Capture phase, because a `<script>` or stylesheet that fails to load fires `error` on
  // the element itself and that event does not bubble. Without `true` the failure this
  // exists for would stay invisible.
  window.addEventListener(
    'error',
    function (event) {
      var target = event && event.target;
      if (target && target !== window && (target.src || target.href)) {
        reveal('Could not load ' + (target.src || target.href) + '.');
        return;
      }
      reveal(event && event.message ? String(event.message) : '');
    },
    true,
  );

  window.addEventListener('unhandledrejection', function (event) {
    var reason = event && event.reason;
    reveal(reason && reason.message ? String(reason.message) : reason ? String(reason) : '');
  });

  window.setTimeout(function () {
    if (!settled) reveal('It is taking longer than expected to start.');
  }, WATCHDOG_MS);

  /**
   * Disarm the watchdog. A shell calls this once it has shown a view — the sign-in panel
   * counts, because a page that reached the sign-in panel is not blank.
   */
  window.__ferromaReady = function () {
    settled = true;
  };
})();
