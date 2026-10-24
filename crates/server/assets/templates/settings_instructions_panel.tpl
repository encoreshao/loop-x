
<section class="card">
<div class="section-header">⟦_SECTION_ICON_INSTRUCTIONS⟧<h2>⟦html.escape(_t('Your instructions'))⟧</h2></div>
<p class="section-subtitle">⟦instructions_subtitle⟧</p>
<form method='post' action='/instructions' class='daemon-action-form'>
⟦csrf_input⟧
<textarea name='instructions' class='instructions-textarea' rows='24' placeholder="⟦html.escape(_t('e.g. Prefer descriptive commit messages. Never touch files under vendor/.'))⟧">⟦html.escape(current_text)⟧</textarea>
<button type='submit' class='btn btn-primary'>⟦html.escape(_t('Save'))⟧</button>
</form>
</section>
