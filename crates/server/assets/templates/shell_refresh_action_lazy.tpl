var indicator = document.getElementById('gitlab-refresh-indicator');
      if (indicator) { indicator.style.display = ''; }
      Promise.all(Array.prototype.map.call(
        document.querySelectorAll('[data-lazy-load]'), window.__loopLoadLazyContent
      )).then(function() {
        if (indicator) { indicator.style.display = 'none'; }
        __loopScheduleRefresh();
      });