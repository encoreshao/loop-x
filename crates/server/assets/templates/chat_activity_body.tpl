
<div class="page-title">
<h1>⟦html.escape(_t('Activity'))⟧</h1>
<p class="subtitle">⟦activity_subtitle⟧</p>
</div>

⟦flash_html⟧

<div class="overview-layout">
<div class="activity-card-stack">
<div class='card'>
<div class="section-header">⟦_SECTION_ICON_GITLAB⟧<h2>⟦html.escape(_t('GitLab Monitor'))⟧</h2></div>
⟦state_hero_html⟧
<ul class="field-list">⟦''.join(status_lines)⟧</ul>
⟦gitlab_run_now_html⟧
⟦log_html⟧
</div>
<div class='card'>
<div class="section-header">⟦_SECTION_ICON_TOPIC_MONITOR⟧<h2>⟦html.escape(_t('Topic Monitor'))⟧</h2></div>
⟦topic_state_hero_html⟧
<ul class="field-list">⟦''.join(topic_fields)⟧</ul>
⟦topic_run_now_html⟧
</div>
</div>

<div class="activity-card-stack">
<div class="card">
<div class="section-header">⟦_SECTION_ICON_OVERVIEW⟧<h2>⟦html.escape(_t('Latest Run Review'))⟧</h2></div>
<div class="markdown">⟦render_markdown(review)⟧</div>
</div>
<div class="card">
<div class="section-header">⟦_SECTION_ICON_TOPIC_MONITOR⟧<h2>⟦html.escape(_t('Latest Topic Run Review'))⟧</h2></div>
⟦_topic_latest_data_html(topics)⟧
</div>
</div>
</div>
