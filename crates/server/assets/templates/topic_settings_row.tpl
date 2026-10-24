
<div class='project-block topic-settings-row⟦disabled_class⟧'>
<div class='topic-row'>
⟦_topic_action_html(topic, csrf_input)⟧
<form method='post' action='/topic-monitor/topics' class='daemon-action-form topic-row-fields' id='⟦edit_form_id⟧'>
⟦csrf_input⟧
<input type='hidden' name='original_name' value='⟦safe_name⟧'>
<div class='topic-row-line1'>
<input type='text' name='label' value='⟦html.escape(topic.get('label', ''))⟧' placeholder='⟦html.escape(_t('label'))⟧' required>
<input type='text' name='name' value='⟦safe_name⟧' placeholder='⟦html.escape(_t('topic name'))⟧' class='topic-row-name-input' required>
⟦_custom_select('slack_bundle', bundles, topic.get('slack_bundle') or '', empty_label=_t('(use default webhook)'))⟧
</div>
<textarea name='brief' rows='2' placeholder='⟦html.escape(_t('what counts as notable'))⟧' class='topic-row-brief' required>⟦html.escape(topic.get('brief', ''))⟧</textarea>
</form>
<div class='topic-row-actions'>
<button type='submit' form='⟦edit_form_id⟧' class='btn btn-neutral'>
<span class='material-symbols-outlined' aria-hidden='true'>save</span> ⟦html.escape(_t('Save'))⟧</button>
<form method='post' action='/topic-monitor/topics/⟦url_safe_name⟧/delete' id='⟦delete_form_id⟧'>⟦csrf_input⟧</form>
<button type='submit' form='⟦delete_form_id⟧' class='btn btn-warning' data-confirm="⟦delete_confirm⟧">
<span class='material-symbols-outlined' aria-hidden='true'>delete</span> ⟦html.escape(_t('Delete'))⟧</button>
</div>
</div>
</div>
