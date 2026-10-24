
<section class="card">
<div class="section-header">⟦_SECTION_ICON_AI_CLI⟧<h2>⟦html.escape(_t('Selected CLI'))⟧</h2></div>
<p class="section-subtitle">⟦html.escape(_t('Choose which AI CLI tool the GitLab issue loop and the Topic Monitor loop both use.'))⟧</p>
<p><strong>⟦html.escape(_t('Currently:'))⟧</strong> ⟦html.escape(cli_labels[current_cli])⟧</p>
<form method='post' action='/ai-cli' class='daemon-action-form single-field'>
⟦csrf_input⟧
⟦select_html⟧
<button type='submit' class='btn btn-neutral'>⟦html.escape(_t('Save'))⟧</button>
</form>
</section>
