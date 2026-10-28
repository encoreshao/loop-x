
<div class="page-title">
<h1>⟦html.escape(_t('Activity'))⟧</h1>
<p class="subtitle">⟦activity_subtitle⟧</p>
</div>

⟦flash_html⟧

<div class="activity-sections">
⟦progress_html⟧
⟦loops_html⟧
<div class="activity-reviews">
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
