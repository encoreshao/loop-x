
  var refreshSeconds = parseInt(localStorage.getItem('loop-dashboard-refresh-interval'), 10) || 30;
  (function __loopScheduleRefresh() {
    setTimeout(function() {
      if (window.__loopChatStreaming) { __loopScheduleRefresh(); return; }
      ⟦refresh_action⟧
    }, refreshSeconds * 1000);
  })();