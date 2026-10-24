
<p class="section-subtitle">⟦html.escape(_t('Appearance settings for this browser - saved locally, not shared across devices.'))⟧</p>

<div class="grid">
<section class="card">
<div class="section-header">⟦_SECTION_ICON_PREFERENCES⟧<h2>⟦html.escape(_t('Color mode'))⟧</h2></div>
<p class="section-subtitle">⟦html.escape(_t('Choose how the interface looks - light, dark, or match your system setting.'))⟧</p>
<div class="pref-segmented" role="group" aria-label="⟦html.escape(_t('Color mode'))⟧">⟦mode_buttons⟧</div>
</section>
</div>

<div class="grid">
<section class="card">
<div class="section-header"><span class="pref-theme-icon">⟦_SECTION_ICON_PREFERENCES⟧</span><h2>⟦html.escape(_t('Theme'))⟧</h2></div>
<p class="section-subtitle">⟦html.escape(_t('Select the accent color for the application interface.'))⟧</p>
<div class="pref-swatches" role="group" aria-label="⟦html.escape(_t('Accent color'))⟧">⟦swatch_buttons⟧</div>
</section>
</div>

<div class="grid">
<section class="card">
<div class="section-header">⟦_SECTION_ICON_PREFERENCES⟧<h2>⟦html.escape(_t('Font'))⟧</h2></div>
<p class="section-subtitle">⟦html.escape(_t('Choose the typeface used across the dashboard.'))⟧</p>
<div class="pref-segmented" role="group" aria-label="⟦html.escape(_t('Font'))⟧">⟦font_buttons⟧</div>
</section>
</div>

<div class="grid">
<section class="card">
<div class="section-header">⟦_SECTION_ICON_PREFERENCES⟧<h2>⟦html.escape(_t('Auto-refresh'))⟧</h2></div>
<p class="section-subtitle">⟦html.escape(_t('How often pages reload themselves to show live status.'))⟧</p>
<div class="pref-segmented" role="group" aria-label="⟦html.escape(_t('Auto-refresh interval'))⟧">⟦refresh_buttons⟧</div>
</section>
</div>

<script>
(function() {
  function apply() {
    var mode = localStorage.getItem('loop-dashboard-color-mode') || 'auto';
    var accent = localStorage.getItem('loop-dashboard-accent') || 'indigo';
    var font = localStorage.getItem('loop-dashboard-font') || 'roboto';
    var refreshSeconds = localStorage.getItem('loop-dashboard-refresh-interval') || '30';
    document.querySelectorAll('[data-color-mode-choice]').forEach(function(btn) {
      btn.classList.toggle('is-active', btn.getAttribute('data-color-mode-choice') === mode);
    });
    document.querySelectorAll('[data-accent-choice]').forEach(function(btn) {
      btn.classList.toggle('is-active', btn.getAttribute('data-accent-choice') === accent);
    });
    document.querySelectorAll('[data-font-choice]').forEach(function(btn) {
      btn.classList.toggle('is-active', btn.getAttribute('data-font-choice') === font);
    });
    document.querySelectorAll('[data-refresh-choice]').forEach(function(btn) {
      btn.classList.toggle('is-active', btn.getAttribute('data-refresh-choice') === refreshSeconds);
    });
  }
  document.querySelectorAll('[data-color-mode-choice]').forEach(function(btn) {
    btn.addEventListener('click', function() {
      var mode = btn.getAttribute('data-color-mode-choice');
      if (mode === 'auto') {
        localStorage.removeItem('loop-dashboard-color-mode');
        document.documentElement.removeAttribute('data-color-mode');
      } else {
        localStorage.setItem('loop-dashboard-color-mode', mode);
        document.documentElement.setAttribute('data-color-mode', mode);
      }
      apply();
    });
  });
  document.querySelectorAll('[data-accent-choice]').forEach(function(btn) {
    btn.addEventListener('click', function() {
      var accent = btn.getAttribute('data-accent-choice');
      localStorage.setItem('loop-dashboard-accent', accent);
      document.documentElement.setAttribute('data-accent', accent);
      apply();
    });
  });
  document.querySelectorAll('[data-font-choice]').forEach(function(btn) {
    btn.addEventListener('click', function() {
      var font = btn.getAttribute('data-font-choice');
      localStorage.setItem('loop-dashboard-font', font);
      document.documentElement.setAttribute('data-font', font);
      apply();
    });
  });
  document.querySelectorAll('[data-refresh-choice]').forEach(function(btn) {
    btn.addEventListener('click', function() {
      localStorage.setItem('loop-dashboard-refresh-interval', btn.getAttribute('data-refresh-choice'));
      apply();
    });
  });
  apply();
})();
</script>
