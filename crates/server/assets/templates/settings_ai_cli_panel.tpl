
<section class="card" data-ai-cli-panel>
<div class="section-header">⟦_SECTION_ICON_AI_CLI⟧<h2>⟦html.escape(_t('Selected CLI'))⟧</h2></div>
<p class="section-subtitle">⟦html.escape(_t('Choose which AI CLI every loop, Inbox Triage and the dashboard chat use.'))⟧</p>
<p><strong>⟦html.escape(_t('Currently:'))⟧</strong> ⟦html.escape(cli_labels[current_cli])⟧</p>
⟦warnings_html⟧
<form method='post' action='/ai-cli' class='daemon-action-form single-field'>
⟦csrf_input⟧
⟦select_html⟧
<button type='submit' class='btn btn-neutral'>⟦html.escape(_t('Save'))⟧</button>
</form>
<script>
(function() {
  var select = document.querySelector('[data-ai-cli-panel] select[name="cli"]');
  if (!select) return;
  select.addEventListener('change', function() {
    document.querySelectorAll('[data-ai-cli-warning]').forEach(function(box) {
      box.hidden = box.getAttribute('data-ai-cli-warning') !== select.value;
    });
  });
})();
</script>
</section>
