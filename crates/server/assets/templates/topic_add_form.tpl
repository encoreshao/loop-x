
<div class='project-block topic-settings-row'>
<div class='topic-row'>
<div class='topic-row-switch-spacer'></div>
<form method='post' action='/topic-monitor/topics' class='daemon-action-form topic-row-fields' id='topic-add-form'>
⟦csrf_input⟧
<div class='topic-row-line1'>
<input type='text' name='name' placeholder='⟦html.escape(_t('topic name'))⟧' required>
<input type='text' name='label' placeholder='⟦html.escape(_t('label'))⟧' required>
⟦_custom_select('slack_bundle', bundles, None, empty_label=_t('(use default webhook)'))⟧
</div>
<textarea name='brief' rows='2' placeholder='⟦html.escape(_t('what counts as notable'))⟧' class='topic-row-brief' required></textarea>
</form>
<div class='topic-row-actions'>
<button type='submit' form='topic-add-form' class='btn btn-neutral'><span class='material-symbols-outlined' aria-hidden='true'>add</span> ⟦html.escape(_t('Add topic'))⟧</button>
</div>
</div>
</div>
