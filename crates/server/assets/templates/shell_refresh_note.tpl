
(function() {
  document.addEventListener('DOMContentLoaded', function() {
    var refreshSeconds = parseInt(localStorage.getItem('loop-dashboard-refresh-interval'), 10) || 30;
    var labels = {5: '5s', 11: '11s', 30: '30s', 60: '1 min', 300: '5 min'};
    var el = document.getElementById('refresh-note-text');
    if (el) el.textContent = __REFRESH_TEMPLATE__.replace('{interval}', labels[refreshSeconds] || (refreshSeconds + 's'));
  });
})();