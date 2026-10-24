<aside class='ai-panel' id='ai-panel' data-page='⟦html.escape(active_page, quote=True)⟧' aria-label='⟦panel_label⟧'>
<div class='ai-panel-resizer' role='separator' aria-orientation='vertical' tabindex='0' aria-label='⟦html.escape(_t('Resize assistant panel'))⟧' aria-valuemin='320' aria-valuemax='720' aria-valuenow='400'></div>
<div class='ai-panel-header'>
<span class='ai-panel-title'><span class='material-symbols-outlined' aria-hidden='true'>auto_awesome</span>Loop X</span>
<div class='ai-panel-header-actions'>
<button type='button' class='ai-panel-icon-btn' data-ai-history-toggle aria-controls='ai-panel-history' aria-expanded='false' aria-label='⟦history⟧' title='⟦history⟧'><span class='material-symbols-outlined' aria-hidden='true'>history</span></button>
<button type='button' class='ai-panel-icon-btn' data-ai-new-chat aria-label='⟦new_chat⟧' title='⟦new_chat⟧'><span class='material-symbols-outlined' aria-hidden='true'>add_comment</span></button>
<button type='button' class='ai-panel-icon-btn' data-ai-close aria-label='⟦close⟧' title='⟦close⟧'><span class='material-symbols-outlined' aria-hidden='true'>close</span></button>
</div>
</div>
<div class='ai-panel-history' id='ai-panel-history' aria-label='⟦history⟧' hidden>
<h3 class='ai-panel-history-heading'>⟦html.escape(_t('Chats'))⟧</h3>
<div class='ai-panel-history-list' id='ai-panel-history-list'></div>
</div>
<div class='ai-panel-body' id='ai-panel-body'>
<div class='ai-panel-empty'>
<div class='ai-panel-greeting'>⟦html.escape(_t('Hi, how can I help?'))⟧</div>
⟦context_html⟧
</div>
<div class='ai-panel-thread' id='ai-panel-thread' aria-live='polite'></div>
</div>
<div class='ai-panel-footer'>
<div class='ai-panel-prompts'>⟦prompts_html⟧</div>
<form class='ai-panel-composer' id='ai-panel-form'>
<input type='hidden' name='csrf_token' value="⟦html.escape(_CSRF_TOKEN)⟧">
<textarea name='text' rows='1' placeholder='⟦html.escape(_t('Ask about this page, or paste a GitLab issue link'))⟧' aria-label='⟦html.escape(_t('Message the assistant'))⟧'></textarea>
<div class='ai-panel-composer-toolbar'>
<button type='submit' class='ai-panel-send' aria-label='⟦html.escape(_t('Send'))⟧' title='⟦html.escape(_t('Send'))⟧' disabled><span class='material-symbols-outlined' aria-hidden='true'>arrow_upward</span></button>
</div>
</form>
<p class='ai-panel-disclaimer'>⟦html.escape(_t('Loop X can make mistakes - double-check before acting on it.'))⟧</p>
</div>
</aside>