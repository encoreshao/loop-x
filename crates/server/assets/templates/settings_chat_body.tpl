
<div class="page-header">
<h1>⟦html.escape(_t('Chat tools'))⟧</h1>
<p class="subtitle">⟦html.escape(_t('Read tools run straight away. Tools that create, comment or send wait for your Approve click in the chat. Tick a tool to skip that review - it then runs as soon as the assistant calls it.'))⟧</p>
</div>

⟦flash_html⟧

<div class="grid">
<section class="card">
<div class="section-header"><h2>⟦html.escape(_t('Skip review for these actions'))⟧</h2></div>
<form method="POST" action="/settings/chat-tools">
⟦csrf_input⟧
<ul class="plain-list">⟦rows⟧</ul>
<button type="submit" class="btn btn-primary">⟦html.escape(_t('Save'))⟧</button>
</form>
</section>
</div>
