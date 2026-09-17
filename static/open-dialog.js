/* Puts the per-release "Open" options in a modal instead of a row of inputs
   inside the results table (which widened every row and made the table
   scroll sideways on a phone). Vendored into the binary; served from
   /open-dialog.js.

   The row keeps its data in hidden inputs and its button just calls
   `openDialogFrom(this)`, so the release's magnet/indexer/source_url travel
   with the form the dialog actually submits -- no data duplicated into
   attributes, and no client-side state to keep in sync. */

function openDialogFrom(button) {
  var row = button.closest('form');
  var dialog = document.getElementById('open-dialog');
  var template = document.getElementById('open-form-template');
  if (!row || !dialog || !template || typeof dialog.showModal !== 'function') {
    return;
  }

  var body = document.getElementById('open-dialog-body');
  // Re-clone every time: the previous submit's inputs (and any htmx state)
  // must not leak into the next release we open.
  body.replaceChildren(template.content.cloneNode(true));

  var form = body.querySelector('form.open-form');
  // `title` travels too: it names the per-torrent subfolder when the
  // "own folder" box is ticked for a login-walled indexer's .torrent.
  ['magnet', 'indexer', 'source_url', 'title'].forEach(function (name) {
    var value = row.querySelector('input[name="' + name + '"]');
    if (value) {
      var hidden = document.createElement('input');
      hidden.type = 'hidden';
      hidden.name = name;
      hidden.value = value.value;
      form.appendChild(hidden);
    }
  });

  // Naming the release makes it obvious which row this dialog belongs to.
  // `about` sits *outside* the form in the template, so look it up on the
  // dialog body, not on `form`.
  var title = row.querySelector('input[name="title"]');
  var about = body.querySelector('p.about');
  if (about && title) {
    about.textContent = title.value;
  }

  // htmx ignores content inside a <template> (it never processes markup
  // that isn't in the document), so without this the cloned form's hx-post
  // and hx-on:click attributes are inert: Cancel did nothing and "Open
  // torrent" fell back to a native submit that reloaded the page. Must run
  // after the clone is in the document.
  if (window.htmx && typeof window.htmx.process === 'function') {
    window.htmx.process(form);
  }

  dialog.showModal();
  var directory = form.querySelector('input[name="directory"]');
  if (directory) {
    directory.focus();
    directory.select();
  }
}

function openDialogClose() {
  var dialog = document.getElementById('open-dialog');
  if (dialog && dialog.open) {
    dialog.close();
  }
}

/* Runs after /open responds. The file list has been swapped into #player by
   then, so a success closes the dialog and scrolls the player into view. A
   failure leaves the dialog open with what was typed still in it (htmx
   discards error responses, so the reason is shown here instead). */
function openDialogAfterRequest(event) {
  var detail = event.detail || {};
  var xhr = detail.xhr;
  var dialog = document.getElementById('open-dialog');
  var error = dialog && dialog.querySelector('p.error');

  if (!xhr || xhr.status >= 400 || !detail.successful) {
    if (error) {
      error.hidden = false;
      error.textContent = xhr && xhr.responseText
        ? 'Could not open this torrent: ' + xhr.responseText.replace(/^error:\s*/, '').slice(0, 300)
        : 'Could not open this torrent.';
    }
    return;
  }

  if (error) {
    error.hidden = true;
    error.textContent = '';
  }
  openDialogClose();
  var player = document.getElementById('player');
  if (player && player.scrollIntoView) {
    player.scrollIntoView({ behavior: 'smooth', block: 'start' });
  }
}
