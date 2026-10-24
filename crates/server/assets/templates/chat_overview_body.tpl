
<div class='chat-page⟦' is-empty' if is_empty else ''⟧'>
<div class='chat-toolbar'>
<button type='button' class='btn btn-neutral chat-tool-btn' data-chat-history-open aria-controls='chat-history' aria-expanded='false'><span class='material-symbols-outlined' aria-hidden='true'>history</span>⟦html.escape(_t('History'))⟧</button>
<form method='post' action='/activity/new-chat' class='chat-new-form'>
⟦csrf_input⟧
<button type='submit' class='btn btn-neutral chat-tool-btn chat-new-btn'><span class='material-symbols-outlined' aria-hidden='true'>add_comment</span>⟦html.escape(_t('New chat'))⟧</button>
</form>
</div>
<aside class='doc-panel' id='doc-panel' aria-label='⟦html.escape(_t('Document'))⟧'>
<div class='doc-panel-header'>
<span class='doc-panel-title' data-doc-title></span>
<button type='button' class='btn btn-neutral' data-doc-copy><span class='material-symbols-outlined' aria-hidden='true' data-doc-copy-icon>content_copy</span><span data-doc-copy-label>⟦html.escape(_t('Copy'))⟧</span></button>
<a class='btn btn-neutral' data-doc-download href='#' download><span class='material-symbols-outlined' aria-hidden='true'>download</span>⟦html.escape(_t('Download'))⟧</a>
<button type='button' class='message-action-btn' data-doc-close aria-label='⟦html.escape(_t('Close document'))⟧'><span class='material-symbols-outlined' aria-hidden='true'>close</span></button>
</div>
<div class='doc-panel-body' data-doc-body></div>
</aside>
<div class='chat-history-backdrop' data-chat-history-close hidden></div>
<aside class='chat-history' id='chat-history' aria-label='⟦html.escape(_t('Chat history'))⟧' hidden>
<div class='chat-history-header'>
<h2>⟦html.escape(_t('Chats'))⟧</h2>
<button type='button' class='message-action-btn' data-chat-history-close aria-label='⟦html.escape(_t('Close history'))⟧'><span class='material-symbols-outlined' aria-hidden='true'>close</span></button>
</div>
<div id='chat-history-list'>⟦history_html⟧</div>
</aside>
⟦flash_html⟧
<div class='chat-hero'>
<a class='chat-announce' href='/?view=activity'><span class='material-symbols-outlined' aria-hidden='true'>auto_awesome</span><span class='chat-announce-text'>⟦html.escape(announce_text)⟧</span><span class='material-symbols-outlined chat-announce-arrow' aria-hidden='true'>arrow_forward</span></a>
<h1 class='chat-hero-title'>⟦hero_title_html⟧</h1>
</div>
<div class='chat-thread activity-messages-grid'>
⟦messages_html⟧
</div>
<div class='activity-composer'>
<div class='activity-composer-inner'>
<form method='post' action='/activity/messages' class='activity-composer-form' id='activity-composer-form'>
⟦csrf_input⟧
⟦session_input⟧
<textarea name='text' class='activity-composer-input' rows='2' placeholder='⟦html.escape(_t('Ask the loop anything, or paste a GitLab issue link'))⟧' aria-label='⟦html.escape(_t('Message the loop'))⟧' required></textarea>
<div class='chat-composer-toolbar'>
<div class='chat-chips'>⟦suggestions_html⟧</div>
<button type='submit' class='chat-send-btn' aria-label='⟦html.escape(_t('Send'))⟧'><span class='material-symbols-outlined' aria-hidden='true'>arrow_upward</span></button>
</div>
</form>
</div>
</div>
<div class='chat-hero-links'>
<a class='btn btn-neutral chat-link-pill' href='/?view=activity'><span class='material-symbols-outlined' aria-hidden='true'>bolt</span>⟦html.escape(_t('Loop activity'))⟧</a>
<a class='btn btn-neutral chat-link-pill' href='/loops/gitlab-loop'><span class='material-symbols-outlined' aria-hidden='true'>merge</span>⟦html.escape(_t('Live GitLab'))⟧</a>
⟦_overview_live_links_html()⟧
</div>
</div>
