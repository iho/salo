/* Keeps the torrents page's live figures current without reloading it.
 *
 * Polls `/torrents/stats` (one JSON request covering every row) and
 * writes only the speed/ratio/progress *text* of each cell, located by
 * its `data-info-hash`. Nothing else on the page is touched: the
 * seed-limit inputs the user may be typing into keep their values and
 * their focus, and no element is created, moved or replaced.
 *
 * This is why the page uses a JSON poll rather than an htmx fragment
 * swap -- swapping table markup would rebuild the forms and wipe input.
 *
 * Stops polling when the tab is hidden (a background page has no need for
 * 2s-old numbers) and catches up on becoming visible again.
 */

(function () {
  var POLL_MS = 2000;
  var timer = null;

  function els(selector) {
    return Array.prototype.slice.call(document.querySelectorAll(selector));
  }

  function setText(selector, value) {
    els(selector).forEach(function (el) {
      var next = value === null || value === undefined ? '—' : value;
      if (el.textContent !== next) el.textContent = next;
    });
  }

  /* One row's cells, addressed by the info hash so a re-ordered or
   * newly-added torrent can't shift values onto the wrong row. */
  function apply(row) {
    var hash = row.info_hash;
    var q = function (field) {
      return '[data-info-hash="' + hash + '"] [data-field="' + field + '"]';
    };
    var ratio = row.ratio === null || row.ratio === undefined ? '—' : row.ratio;
    els(q('ratio')).forEach(function (el) {
      el.textContent = ratio;
      /* Show the ratio climbing green once it passes 1.0 (you have
       * uploaded as much as you downloaded). */
      el.classList.toggle('ratio-good', rank(row.ratio) >= 1);
    });
    els(q('progress')).forEach(function (el) {
      if (row.finished) {
        if (el.textContent.indexOf('done') !== 0) el.textContent = 'done';
      } else {
        el.textContent = row.progress_percent + '%';
      }
    });
    setText(q('download_speed'), row.download_speed);
    setText(q('upload_speed'), row.upload_speed);
    setText(q('uploaded'), row.uploaded);
  }

  function rank(v) {
    if (v === null || v === undefined) return -1;
    return parseFloat(v);
  }

  function tick() {
    timer = null;
    fetch('/torrents/stats', { headers: { Accept: 'application/json' } })
      .then(function (r) {
        if (!r.ok) throw new Error('stats ' + r.status);
        return r.json();
      })
      .then(function (rows) {
        rows.forEach(apply);
        schedule();
      })
      .catch(function () {
        /* A failed poll is not worth an error banner -- keep the last
         * values and try again. */
        schedule();
      });
  }

  function schedule() {
    if (document.hidden || timer) return;
    timer = setTimeout(tick, POLL_MS);
  }

  document.addEventListener('visibilitychange', function () {
    if (!document.hidden) tick();
  });

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', schedule);
  } else {
    schedule();
  }
})();
