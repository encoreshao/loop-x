// Browser notifications for loopx's web mode: watches /api/events and raises a
// desktop notification for a new pending approval or a fresh outcome (MR opened,
// escalation, failure) while the tab is in the background.
(function () {
  if (!window.EventSource || !window.Notification) return;
  var seen = null;
  var KEY = 'loopx.notify.seen';

  function ask() {
    if (Notification.permission === 'default') Notification.requestPermission();
    window.removeEventListener('click', ask);
    window.removeEventListener('keydown', ask);
  }
  window.addEventListener('click', ask);
  window.addEventListener('keydown', ask);

  function show(id, title, body) {
    if (seen[id]) return;
    seen[id] = 1;
    try { sessionStorage.setItem(KEY, JSON.stringify(Object.keys(seen))); } catch (e) {}
    if (seen.__baseline || document.visibilityState === 'visible') return;
    if (Notification.permission !== 'granted') return;
    new Notification(title, { body: body, tag: id });
  }

  function handle(snap) {
    var first = seen === null;
    if (first) {
      seen = {};
      try { (JSON.parse(sessionStorage.getItem(KEY)) || []).forEach(function (k) { seen[k] = 1; }); } catch (e) {}
      seen.__baseline = 1;
    }
    var pending = (snap.approvals && snap.approvals.pending) || [];
    pending.forEach(function (a) {
      show('approval:' + a.id, 'Approval needed', a.title || a.kind || 'A run is waiting for you');
    });
    var at = snap.attention;
    if (at) {
      var label = { mr_opened: 'Merge request opened', escalated: 'Issue escalated', failed: 'Run failed' }[at.kind] || at.kind;
      var where = [at.project, at.issue_iid ? '#' + at.issue_iid : ''].filter(Boolean).join(' ');
      show('attention:' + at.kind + ':' + at.at, label, where + (at.reason ? ' - ' + at.reason : ''));
    }
    if (first) delete seen.__baseline;
  }

  var es = new EventSource('/api/events');
  es.addEventListener('snapshot', function (e) {
    try { handle(JSON.parse(e.data)); } catch (err) {}
  });
})();
