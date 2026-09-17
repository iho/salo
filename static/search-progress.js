/* Drives the per-tracker progress panel on the search page.
 *
 * A merged search takes as long as its slowest tracker, so instead of
 * waiting, the form asks the server to start a search *job*
 * (`/search/start`), which runs every tracker in the background while the
 * page polls `/search/progress/<id>` for a bar and each tracker's
 * response time. When the job finishes, the browser navigates to the real
 * results URL with `?job=<id>`, and the server renders them from the
 * releases the job already collected -- nothing is searched twice.
 *
 * Degrades cleanly: with JS off the form is a plain GET to /search and
 * works exactly as before (just without the progress display).
 */

(function () {
  var pollTimer = null;

  function progressEl() {
    return document.getElementById('search-progress');
  }

  function stopPolling() {
    if (pollTimer) {
      clearTimeout(pollTimer);
      pollTimer = null;
    }
  }

  /* Poll the job: swap in the latest progress HTML, and once the job is
   * done, follow the instruction it returns. */
  function poll(id, resultsUrl) {
    var el = progressEl();
    if (!el) {
      return;
    }
    fetch('/search/progress/' + encodeURIComponent(id), {
      headers: { 'HX-Request': 'true' }
    })
      .then(function (response) {
        if (!response.ok) {
          throw new Error('progress HTTP ' + response.status);
        }
        return response.text();
      })
      .then(function (body) {
        // A finished job answers with a script tag-style call instead of
        // markup; anything else is the progress fragment.
        if (body.indexOf('__saloFinishSearch') !== -1) {
          stopPolling();
          el.innerHTML = '<div class="progress-head"><strong>Done.</strong> Loading results&hellip;</div>';
          window.location.href = resultsUrl;
          return;
        }
        el.innerHTML = body;
        pollTimer = setTimeout(function () {
          poll(id, resultsUrl);
        }, 400);
      })
      .catch(function (err) {
        // Don't spam: show once, stop polling, and let the plain search
        // still work if the user resubmits.
        stopPolling();
        el.innerHTML = '<div class="progress-head">Progress unavailable (' + err.message + ')</div>';
      });
  }

  /* Called by the script `/search/start` returns. */
  window.__saloStartSearch = function (id) {
    var form = document.querySelector('form.search');
    var resultsUrl = form ? buildResultsUrl(form, id) : '/search?job=' + encodeURIComponent(id);
    poll(id, resultsUrl);
  };

  /* Runs after htmx has the job id back from /search/start. The response is
   * a snippet of JS rather than markup (there is nothing to swap -- the
   * progress panel is filled by polling), so it's evaluated here instead of
   * relying on htmx to swap a script tag, which an hx-swap of `none`
   * discards. */
  window.saloAfterStart = function (event) {
    var xhr = event.detail && event.detail.xhr;
    if (!xhr || xhr.status >= 400) {
      var el = progressEl();
      if (el) {
        el.innerHTML = '<div class="progress-head">Could not start the search' +
          (xhr ? ' (HTTP ' + xhr.status + ')' : '') + '</div>';
      }
      return;
    }
    // The body is `window.__saloStartSearch("...")`; run it.
    try {
      (0, eval)(xhr.responseText);
    } catch (err) {
      var el2 = progressEl();
      if (el2) {
        el2.innerHTML = '<div class="progress-head">Progress failed: ' + err.message + '</div>';
      }
    }
  };

  /* The results URL the job's releases should be rendered at: exactly what
   * the form would submit, plus the job id so /search reuses them. */
  function buildResultsUrl(form, id) {
    var params = new URLSearchParams(new FormData(form));
    params.set('job', id);
    return '/search?' + params.toString();
  }

  /* Check every tracker checkbox (or none). */
  window.toggleAllTrackers = function (checked) {
    var boxes = document.querySelectorAll('form.search input[name="trackers"]');
    for (var i = 0; i < boxes.length; i++) {
      boxes[i].checked = checked;
    }
  };
})();
