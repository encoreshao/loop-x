(function () {
  // Video dialogs: a poster's [data-cr-open] opens its <dialog>; the video's
  // src is set from data-src only then, and cleared again on close so nothing
  // keeps downloading or playing. Esc closes natively; a click on the backdrop
  // (the dialog element itself, since its content fills it) closes too.
  if (window.__loopxCreator) return;
  window.__loopxCreator = true;

  function open(dialog, opener) {
    var video = dialog.querySelector('video[data-src]');
    // A template's Play sample button names its clip and title; one popup serves them all.
    var clip = opener && opener.getAttribute('data-tpl-src');
    if (video && clip) {
      video.setAttribute('data-src', clip);
      video.removeAttribute('src');
      var heading = dialog.querySelector('h2');
      if (heading) heading.textContent = opener.getAttribute('data-tpl-title') || '';
    }
    if (video && !video.getAttribute('src')) video.setAttribute('src', video.getAttribute('data-src'));
    dialog.__opener = opener || null;
    if (typeof dialog.showModal === 'function') dialog.showModal();
    else dialog.setAttribute('open', '');
    if (video && clip) { var p = video.play(); if (p && p.catch) p.catch(function () {}); }
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
    dialog.querySelectorAll('details.cr-menu[open]').forEach(function (m) { m.removeAttribute('open'); });
    if (dialog.__opener && document.contains(dialog.__opener)) dialog.__opener.focus();
  }

  // The More menu closes on an outside click and on Escape (before the dialog does).
  document.addEventListener('click', function (e) {
    document.querySelectorAll('details.cr-menu[open]').forEach(function (m) {
      if (!m.contains(e.target)) m.removeAttribute('open');
    });
  });
  document.addEventListener('keydown', function (e) {
    if (e.key !== 'Escape') return;
    var m = document.querySelector('details.cr-menu[open]');
    if (m) { m.removeAttribute('open'); e.preventDefault(); e.stopPropagation(); }
  }, true);

  // One publish group open at a time. The `name` attribute does this in
  // current browsers; this covers the rest.
  document.addEventListener('toggle', function (e) {
    var d = e.target;
    if (!d || !d.matches || !d.matches('details.cr-group[open]')) return;
    var name = d.getAttribute('name');
    if (!name) return;
    document.querySelectorAll('details.cr-group[open]').forEach(function (o) {
      if (o !== d && o.getAttribute('name') === name) o.removeAttribute('open');
    });
  }, true);

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
  // the link's own href (as server-sanitized HTML via ?format=html) into it. While the job is still
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
    fetch(url + (url.indexOf('?') < 0 ? '?' : '&') + 'format=html', { cache: 'no-store', credentials: 'same-origin' })
      .then(function (r) { if (!r.ok) throw new Error(String(r.status)); return r.text(); })
      .then(function (text) {
        if (url !== sheetUrl) return;
        var atEnd = sheetLog.scrollHeight - sheetLog.scrollTop - sheetLog.clientHeight < 24;
        sheet.classList.remove('is-loading');
        if (text.trim()) sheetLog.innerHTML = text;
        else sheetLog.textContent = sheet.getAttribute('data-empty-label');
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

  // Copy the whole log (what is shown, as plain text); the button says "Copied" for a moment.
  function copyLog(button) {
    var text = sheetLog.textContent;
    if (sheet.classList.contains('is-loading') || !text) return;
    var done = function () {
      var label = button.getAttribute('aria-label');
      button.classList.add('is-copied');
      button.setAttribute('aria-label', sheet.getAttribute('data-copied-label'));
      button.title = sheet.getAttribute('data-copied-label');
      setTimeout(function () {
        button.classList.remove('is-copied');
        button.setAttribute('aria-label', label);
        button.title = label;
      }, 1600);
    };
    var fallback = function () {
      var area = document.createElement('textarea');
      area.value = text;
      area.setAttribute('readonly', '');
      area.style.position = 'fixed';
      area.style.opacity = '0';
      sheet.appendChild(area);
      area.select();
      try { if (document.execCommand('copy')) done(); } catch (e) { /* nothing to do */ }
      area.remove();
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, fallback);
    } else {
      fallback();
    }
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
      var copy = t.closest && t.closest('[data-cr-copy]');
      if (copy) { copyLog(copy); return; }
      if (t === sheet) sheet.close();
    });
  }

  // Live job status: every [data-cr-status] row asks for its job's latest step every
  // 2s (only while the tab is visible) and the page reloads once when a job is no
  // longer running, so it moves to the right place (Videos, Jobs history, ...).
  var statusRows = document.querySelectorAll('[data-cr-status]');
  if (statusRows.length) {
    var reloading = false;
    var pollStatus = function () {
      if (document.hidden || reloading) return;
      statusRows.forEach(function (row) {
        var id = row.getAttribute('data-cr-status');
        fetch('/creator/jobs/' + encodeURIComponent(id) + '/status', { cache: 'no-store', credentials: 'same-origin' })
          .then(function (r) { if (!r.ok) throw new Error(String(r.status)); return r.json(); })
          .then(function (v) {
            if (v.state !== 'running') { reloading = true; window.location.reload(); return; }
            var bar = document.querySelector('[data-cr-progress="' + id + '"]');
            if (bar && v.progress) {
              bar.querySelector('.cr-progress-text').textContent = v.progress.text;
              bar.querySelector('.cr-progress-pct').textContent = v.progress.percent + '%';
              var track = bar.querySelector('[role="progressbar"]');
              track.setAttribute('aria-valuenow', String(v.progress.percent));
              track.setAttribute('aria-label', v.progress.text);
              track.firstElementChild.style.width = v.progress.percent + '%';
            }
            var text = v.line || row.getAttribute('data-empty-label');
            if (row.textContent !== text) row.textContent = text;
          })
          .catch(function () {});
      });
    };
    setInterval(pollStatus, 2000);
  }

  // Delete / Clear: the first click arms the button ("Click again to confirm"), the
  // second submits; it disarms itself after 4s.
  document.addEventListener('click', function (ev) {
    var button = ev.target.closest && ev.target.closest('[data-cr-confirm]');
    if (!button) return;
    if (button.getAttribute('data-armed') === 'true') return;
    ev.preventDefault();
    var original = button.innerHTML;
    button.setAttribute('data-armed', 'true');
    button.classList.add('is-armed');
    button.textContent = button.getAttribute('data-confirm-label');
    setTimeout(function () {
      button.removeAttribute('data-armed');
      button.classList.remove('is-armed');
      button.innerHTML = original;
    }, 4000);
  });

  // Share: the Mac app hands the file and text to the native share sheet (AirDrop,
  // Messages, Mail, ...); a plain browser uses the Web Share API, with the video as a
  // file when the browser can share files. Share buttons stay hidden where neither
  // exists, and Open folder (a form) works everywhere.
  var nativeShare = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.loopxShare;
  var webShare = typeof navigator.share === 'function';
  if (nativeShare || webShare) {
    document.querySelectorAll('[data-cr-share]').forEach(function (b) { b.hidden = false; });
  }
  document.addEventListener('click', function (ev) {
    var button = ev.target.closest && ev.target.closest('[data-cr-share]');
    if (!button) return;
    var text = button.getAttribute('data-share-text') || '';
    var path = button.getAttribute('data-share-path') || '';
    var url = button.getAttribute('data-share-url') || '';
    if (nativeShare) {
      // Where the button is, so the share sheet opens right next to it.
      var r = button.getBoundingClientRect();
      nativeShare.postMessage({ path: path, text: text, x: r.left, y: r.top, width: r.width, height: r.height });
      return;
    }
    if (!webShare) return;
    var share = function (data) { return navigator.share(data).catch(function () {}); };
    if (!url) { share({ text: text }); return; }
    fetch(url, { credentials: 'same-origin' })
      .then(function (r) { if (!r.ok) throw new Error(String(r.status)); return r.blob(); })
      .then(function (blob) {
        var name = decodeURIComponent(url.split('/').pop() || 'video.mp4');
        var file = new File([blob], name, { type: blob.type || 'video/mp4' });
        if (navigator.canShare && navigator.canShare({ files: [file] })) return share({ files: [file], text: text, title: text });
        return share({ text: text, title: text });
      })
      .catch(function () { share({ text: text }); });
  });

  // Regenerate: a generated source shows View and Regenerate; Regenerate reveals the
  // usual rights checkbox and Generate button under that row.
  document.addEventListener('click', function (ev) {
    var button = ev.target.closest && ev.target.closest('[data-cr-regen]');
    if (!button) return;
    var item = button.closest('li');
    var panel = item && item.querySelector('.cr-regen');
    if (!panel) return;
    var open = panel.hidden;
    panel.hidden = !open;
    button.setAttribute('aria-expanded', open ? 'true' : 'false');
  });

  // Library: one switch confirms the rights for every Generate / Regenerate on the page
  // (their buttons and hidden rights fields start disabled), and the filter selects
  // apply as soon as they change.
  var rightsToggle = document.querySelector('[data-cr-rights-toggle]');
  if (rightsToggle) {
    rightsToggle.addEventListener('change', function () {
      document.querySelectorAll('[data-cr-rights], [data-cr-needs-rights]').forEach(function (el) {
        el.disabled = !rightsToggle.checked;
      });
    });
  }
  document.querySelectorAll('form[data-cr-autosubmit] select').forEach(function (select) {
    select.addEventListener('change', function () { select.form.requestSubmit(); });
  });

  // Elapsed time of running jobs: every [data-cr-since] ticks once a second.
  var elapsedRows = document.querySelectorAll('[data-cr-since]');
  if (elapsedRows.length) {
    var fmt = function (secs) {
      if (secs >= 3600) return Math.floor(secs / 3600) + 'h ' + String(Math.floor((secs % 3600) / 60)).padStart(2, '0') + 'm';
      if (secs >= 60) return Math.floor(secs / 60) + 'm ' + String(secs % 60).padStart(2, '0') + 's';
      return secs + 's';
    };
    setInterval(function () {
      elapsedRows.forEach(function (row) {
        var secs = Math.max(0, Math.floor((Date.now() - Number(row.getAttribute('data-cr-since'))) / 1000));
        row.textContent = row.getAttribute('data-label').replace('{time}', fmt(secs));
      });
    }, 1000);
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
