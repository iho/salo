/* Theme switch, vendored into the binary (served from /theme.js).

   Loaded synchronously in <head>, so `data-theme` is on <html> before the
   first paint -- no flash of the wrong theme. An explicit choice in
   localStorage wins; with nothing stored the OS/browser preference
   applies, and later OS changes are followed until a choice is made. */
(function () {
  var KEY = 'salo-theme';
  var root = document.documentElement;
  var media = window.matchMedia
    ? window.matchMedia('(prefers-color-scheme: dark)')
    : null;

  function stored() {
    try {
      return localStorage.getItem(KEY);
    } catch (e) {
      return null;
    }
  }

  function system() {
    return media && media.matches ? 'dark' : 'light';
  }

  function current() {
    return stored() || system();
  }

  function paint(theme) {
    root.setAttribute('data-theme', theme);
  }

  function labelFor(theme) {
    return theme === 'dark' ? 'Switch to light theme' : 'Switch to dark theme';
  }

  function syncButton() {
    var btn = document.getElementById('theme-toggle');
    if (!btn) {
      return null;
    }
    var theme = current();
    btn.textContent = theme === 'dark' ? '\u2600' : '\u263E';
    btn.setAttribute('aria-label', labelFor(theme));
    btn.title = labelFor(theme);
    return btn;
  }

  paint(current());

  function onReady() {
    var btn = syncButton();
    if (btn) {
      btn.addEventListener('click', function () {
        var next = current() === 'dark' ? 'light' : 'dark';
        try {
          localStorage.setItem(KEY, next);
        } catch (e) {
          /* private mode: theme just won't persist across pages */
        }
        paint(next);
        syncButton();
      });
    }

    if (media) {
      var onSystemChange = function () {
        if (!stored()) {
          paint(system());
          syncButton();
        }
      };
      if (media.addEventListener) {
        media.addEventListener('change', onSystemChange);
      } else if (media.addListener) {
        media.addListener(onSystemChange);
      }
    }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', onReady);
  } else {
    onReady();
  }
})();
