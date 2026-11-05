(function () {
  // Video dialogs: a poster's [data-cr-open] opens its <dialog>; the video's
  // src is set from data-src only then, and cleared again on close so nothing
  // keeps downloading or playing. Esc closes natively; a click on the backdrop
  // (the dialog element itself, since its content fills it) closes too.
  if (window.__loopxCreator) return;
  window.__loopxCreator = true;

  function open(dialog, opener) {
    var video = dialog.querySelector('video[data-src]');
    if (video && !video.getAttribute('src')) video.setAttribute('src', video.getAttribute('data-src'));
    dialog.__opener = opener || null;
    if (typeof dialog.showModal === 'function') dialog.showModal();
    else dialog.setAttribute('open', '');
    checkClamps(dialog);
  }

  // Show more only where the clamp actually hides text (measured once visible).
  function checkClamps(root) {
    root.querySelectorAll('[data-cr-expand]').forEach(function (button) {
      var text = document.getElementById(button.getAttribute('aria-controls'));
      if (!text || text.classList.contains('is-open') || !text.clientHeight) return;
      button.hidden = text.scrollHeight <= text.clientHeight + 1;
    });
  }

  document.addEventListener('toggle', function (ev) {
    if (ev.target.matches && ev.target.matches('details.cr-group') && ev.target.open) checkClamps(ev.target);
  }, true);

  function close(dialog) {
    if (typeof dialog.close === 'function') dialog.close();
    else { dialog.removeAttribute('open'); onClose(dialog); }
  }

  function onClose(dialog) {
    var video = dialog.querySelector('video');
    if (video) {
      video.pause();
      video.removeAttribute('src');
      video.load();
    }
    if (dialog.__opener && document.contains(dialog.__opener)) dialog.__opener.focus();
  }

  document.querySelectorAll('dialog.cr-dialog').forEach(function (dialog) {
    dialog.addEventListener('close', function () { onClose(dialog); });
  });

  document.addEventListener('click', function (ev) {
    var target = ev.target;
    var opener = target.closest && target.closest('[data-cr-open]');
    if (opener) {
      var dialog = document.getElementById(opener.getAttribute('data-cr-open'));
      if (dialog) open(dialog, opener);
      return;
    }
    var closer = target.closest && target.closest('[data-cr-close]');
    if (closer) {
      var d = closer.closest('dialog');
      if (d) close(d);
      return;
    }
    if (target.tagName === 'DIALOG' && target.classList.contains('cr-dialog')) {
      close(target);
      return;
    }
    var more = target.closest && target.closest('[data-cr-expand]');
    if (more) {
      var text = document.getElementById(more.getAttribute('aria-controls'));
      if (!text) return;
      var expanded = more.getAttribute('aria-expanded') !== 'true';
      text.classList.toggle('is-open', expanded);
      more.setAttribute('aria-expanded', expanded ? 'true' : 'false');
      more.textContent = more.getAttribute(expanded ? 'data-less' : 'data-more');
    }
  });

  // Job log sheet: a [data-cr-log] link opens the one right-hand sheet and loads
  // the link's own href into it as text (never HTML). While the job is still
  // running (no loopx-exit line yet) the log refreshes every 2s. Without script
  // the link just opens the log in a new tab.
  var sheet = document.getElementById('cr-sheet');
  var sheetLog = sheet && sheet.querySelector('.cr-sheet-log');
  var sheetUrl = '';
  var sheetTimer = null;

  function stopPolling() {
    if (sheetTimer) { clearTimeout(sheetTimer); sheetTimer = null; }
  }

  function loadLog(url, first) {
    stopPolling();
    if (first) {
      sheetLog.textContent = sheet.getAttribute('data-loading-label');
      sheet.classList.add('is-loading');
    }
    fetch(url, { cache: 'no-store', credentials: 'same-origin' })
      .then(function (r) { if (!r.ok) throw new Error(String(r.status)); return r.text(); })
      .then(function (text) {
        if (url !== sheetUrl) return;
        var atEnd = sheetLog.scrollHeight - sheetLog.scrollTop - sheetLog.clientHeight < 24;
        sheet.classList.remove('is-loading');
        sheetLog.textContent = text;
        if (first || atEnd) sheetLog.scrollTop = sheetLog.scrollHeight;
        if (sheet.open && text.indexOf('loopx-exit=') === -1) {
          sheetTimer = setTimeout(function () { loadLog(url, false); }, 2000);
        }
      })
      .catch(function () {
        if (url !== sheetUrl) return;
        sheet.classList.remove('is-loading');
        sheetLog.textContent = sheet.getAttribute('data-failed-label');
      });
  }

  if (sheet) {
    sheet.addEventListener('close', function () {
      stopPolling();
      sheetUrl = '';
      if (sheet.__opener && document.contains(sheet.__opener)) sheet.__opener.focus();
    });
    document.addEventListener('click', function (ev) {
      var t = ev.target;
      var link = t.closest && t.closest('[data-cr-log]');
      if (link && typeof sheet.showModal === 'function') {
        ev.preventDefault();
        sheet.__opener = link;
        sheetUrl = link.getAttribute('href');
        if (!sheet.open) sheet.showModal();
        loadLog(sheetUrl, true);
        return;
      }
      if (t.closest && t.closest('[data-cr-refresh]') && sheetUrl) { loadLog(sheetUrl, true); return; }
      if (t === sheet) sheet.close();
    });
  }

  // After Approve or a published toggle the page comes back with ?open=<slug>:
  // reopen that video's dialog.
  var wanted = new URLSearchParams(window.location.search).get('open');
  if (wanted) {
    document.querySelectorAll('dialog.cr-dialog').forEach(function (dialog) {
      if (dialog.getAttribute('data-slug') !== wanted) return;
      open(dialog, document.querySelector('[data-cr-open="' + dialog.id + '"]'));
    });
  }
})();
