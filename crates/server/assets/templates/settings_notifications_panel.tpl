
<section class="card">
<div class="section-header">⟦_SECTION_ICON_SLACK⟧<h2>Slack</h2></div>
<p class="section-subtitle">⟦html.escape(_t('View and manage where this loop sends run notifications. Slack is currently the only channel.'))⟧</p>
<p><strong>⟦html.escape(_t('Default webhook:'))⟧</strong> ⟦webhook_display⟧</p>
<form method='post' action='/notifications/webhook' class='daemon-action-form single-field'>
⟦csrf_input⟧
<input type='password' name='webhook_url' placeholder='⟦html.escape(_t('paste new Slack webhook URL'))⟧' required>
<button type='submit' class='btn btn-neutral'>⟦html.escape(_t('Save'))⟧</button>
</form>
</section>

<section class="card block-kit-card">
<div class="section-header">⟦_SECTION_ICON_BLOCK_KIT_BUILDER⟧<h2>Block Kit Builder</h2></div>
<p class="section-subtitle">⟦bkb_subtitle⟧</p>
<script type="application/json" id="bkb-templates-data">⟦block_templates_json⟧</script>
<script type="application/json" id="bkb-default-only-names-data">⟦default_only_names_json⟧</script>
<div class="block-builder">
  <div class="block-builder-row">
    <label>⟦html.escape(_t('Template'))⟧
      <select id="bkb-template-select"></select>
    </label>
    <label>⟦html.escape(_t('Name'))⟧
      <input type="text" id="bkb-name" placeholder="e.g. run-failed-alert">
    </label>
    <label>⟦html.escape(_t('Bind to notification'))⟧
      <select id="bkb-notification-key">
        <option value="">⟦html.escape(_t('(none)'))⟧</option>
        ⟦notification_key_options⟧
      </select>
    </label>
  </div>
  <div class="block-builder-palette">
    <button type="button" data-add-block="section">+ Section</button>
    <button type="button" data-add-block="header">+ Header</button>
    <button type="button" data-add-block="divider">+ Divider</button>
    <button type="button" data-add-block="context">+ Context</button>
    <button type="button" data-add-block="image">+ Image</button>
    <button type="button" data-add-block="actions">+ Actions</button>
    <button type="button" data-add-block="fields">+ Fields</button>
    <button type="button" data-add-block="markdown">+ Markdown</button>
    <button type="button" data-add-block="carousel">+ Carousel</button>
  </div>
  <div id="bkb-block-list" class="block-builder-list"></div>
  <h3>⟦html.escape(_t('JSON preview'))⟧</h3>
  <pre id="bkb-json-preview" class="block-builder-json"></pre>
  <div class="block-builder-actions">
    <form method="post" action="/notifications/block-templates" class="daemon-action-form single-field" id="bkb-save-form">
    ⟦csrf_input⟧
    <input type="hidden" name="original_name" id="bkb-original-name" value="">
    <input type="hidden" name="name" id="bkb-name-hidden" value="">
    <input type="hidden" name="notification_key" id="bkb-notification-key-hidden" value="">
    <input type="hidden" name="blocks_json" id="bkb-blocks-json" value="">
    <button type="submit" class="btn btn-primary">⟦html.escape(_t('Save'))⟧</button>
    </form>
    <form method="post" action="/notifications/block-templates/placeholder/delete" id="bkb-delete-form">
    ⟦csrf_input⟧
    <button type="submit" class="btn btn-neutral" id="bkb-delete-btn" disabled>⟦html.escape(_t('Delete'))⟧</button>
    </form>
    <form method="post" action="/notifications/block-templates/placeholder/test" id="bkb-test-form">
    ⟦csrf_input⟧
    <button type="submit" class="btn btn-neutral" id="bkb-test-btn" disabled>⟦html.escape(_t('Send test message'))⟧</button>
    </form>
  </div>
</div>
<script>
(function() {
  var templates = JSON.parse(document.getElementById('bkb-templates-data').textContent || '{}');
  var defaultOnlyNames = JSON.parse(document.getElementById('bkb-default-only-names-data').textContent || '[]');
  var templateSelect = document.getElementById('bkb-template-select');
  var nameInput = document.getElementById('bkb-name');
  var notificationKeySelect = document.getElementById('bkb-notification-key');
  var blockList = document.getElementById('bkb-block-list');
  var jsonPreview = document.getElementById('bkb-json-preview');
  var originalNameHidden = document.getElementById('bkb-original-name');
  var nameHidden = document.getElementById('bkb-name-hidden');
  var notificationKeyHidden = document.getElementById('bkb-notification-key-hidden');
  var blocksJsonHidden = document.getElementById('bkb-blocks-json');
  var deleteForm = document.getElementById('bkb-delete-form');
  var testForm = document.getElementById('bkb-test-form');

  var state = { blocks: [] };

  function defaultCarouselCard() {
    return {
      type: 'card',
      hero_image: { type: 'image', image_url: '', alt_text: '' },
      title: { type: 'mrkdwn', text: '' },
      subtitle: { type: 'mrkdwn', text: '' },
      body: { type: 'mrkdwn', text: '' },
      actions: [{ type: 'button', text: { type: 'plain_text', text: '' }, url: '' }]
    };
  }

  function defaultBlock(type) {
    if (type === 'section') return { type: 'section', text: { type: 'mrkdwn', text: '' } };
    if (type === 'header') return { type: 'header', text: { type: 'plain_text', text: '' } };
    if (type === 'divider') return { type: 'divider' };
    if (type === 'context') return { type: 'context', elements: [{ type: 'mrkdwn', text: '' }] };
    if (type === 'image') return { type: 'image', image_url: '', alt_text: '' };
    if (type === 'actions') return { type: 'actions', elements: [{ type: 'button', text: { type: 'plain_text', text: '' }, url: '' }] };
    if (type === 'fields') return { type: 'section', fields: [{ type: 'mrkdwn', text: '' }] };
    if (type === 'markdown') return { type: 'markdown', text: '' };
    if (type === 'carousel') return { type: 'carousel', elements: [defaultCarouselCard()] };
    return { type: type };
  }

  function button(label, onClick) {
    var b = document.createElement('button');
    b.type = 'button';
    b.textContent = label;
    b.addEventListener('click', onClick);
    return b;
  }

  function labelWrap(labelText, input) {
    var label = document.createElement('label');
    label.className = 'block-builder-field';
    var span = document.createElement('span');
    span.textContent = labelText;
    label.appendChild(span);
    label.appendChild(input);
    return label;
  }

  function textInput(labelText, value, onChange) {
    var input = document.createElement('input');
    input.type = 'text';
    input.value = value || '';
    input.addEventListener('input', function() { onChange(input.value); });
    return labelWrap(labelText, input);
  }

  function textArea(labelText, value, onChange) {
    var textarea = document.createElement('textarea');
    textarea.value = value || '';
    textarea.rows = 3;
    textarea.addEventListener('input', function() { onChange(textarea.value); });
    return labelWrap(labelText, textarea);
  }

  function note(text) {
    var p = document.createElement('p');
    p.className = 'block-builder-note';
    p.textContent = text;
    return p;
  }

  function stringList(labelText, values, onValueChange, onStructureChange) {
    var wrap = document.createElement('div');
    wrap.className = 'block-builder-list-field';
    var span = document.createElement('span');
    span.textContent = labelText;
    wrap.appendChild(span);
    values.forEach(function(v, i) {
      var row = document.createElement('div');
      row.className = 'block-builder-list-row';
      var input = document.createElement('input');
      input.type = 'text';
      input.value = v;
      input.addEventListener('input', function() {
        var next = values.slice(); next[i] = input.value; onValueChange(next);
      });
      row.appendChild(input);
      row.appendChild(button('\u2715', function() {
        var next = values.slice(); next.splice(i, 1); onStructureChange(next.length ? next : ['']);
      }));
      wrap.appendChild(row);
    });
    wrap.appendChild(button(⟦json.dumps(_t('+ Add'))⟧, function() { onStructureChange(values.concat([''])); }));
    return wrap;
  }

  function buttonList(elements, onValueChange, onStructureChange) {
    var wrap = document.createElement('div');
    wrap.className = 'block-builder-list-field';
    elements.forEach(function(el, i) {
      var row = document.createElement('div');
      row.className = 'block-builder-list-row';
      row.appendChild(textInput(⟦json.dumps(_t('Label'))⟧, el.text.text, function(v) { el.text.text = v; onValueChange(elements); }));
      row.appendChild(textInput('URL', el.url, function(v) { el.url = v; onValueChange(elements); }));
      row.appendChild(button('\u2715', function() {
        var next = elements.slice(); next.splice(i, 1);
        onStructureChange(next.length ? next : [{ type: 'button', text: { type: 'plain_text', text: '' }, url: '' }]);
      }));
      wrap.appendChild(row);
    });
    wrap.appendChild(button(⟦json.dumps(_t('+ Add button'))⟧, function() {
      onStructureChange(elements.concat([{ type: 'button', text: { type: 'plain_text', text: '' }, url: '' }]));
    }));
    return wrap;
  }

  function cardList(cards, onValueChange, onStructureChange) {
    var wrap = document.createElement('div');
    wrap.className = 'block-builder-list-field';
    cards.forEach(function(card, i) {
      var box = document.createElement('div');
      box.className = 'block-builder-subcard';
      box.appendChild(textInput(⟦json.dumps(_t('Hero image URL'))⟧, card.hero_image.image_url, function(v) { card.hero_image.image_url = v; onValueChange(cards); }));
      box.appendChild(textInput(⟦json.dumps(_t('Hero image alt text'))⟧, card.hero_image.alt_text, function(v) { card.hero_image.alt_text = v; onValueChange(cards); }));
      box.appendChild(textInput(⟦json.dumps(_t('Title'))⟧, card.title.text, function(v) { card.title.text = v; onValueChange(cards); }));
      box.appendChild(textInput(⟦json.dumps(_t('Subtitle'))⟧, card.subtitle.text, function(v) { card.subtitle.text = v; onValueChange(cards); }));
      box.appendChild(textArea(⟦json.dumps(_t('Body ({token} available)', token='{{message}}'))⟧, card.body.text, function(v) { card.body.text = v; onValueChange(cards); }));
      box.appendChild(textInput(⟦json.dumps(_t('Button label'))⟧, card.actions[0].text.text, function(v) { card.actions[0].text.text = v; onValueChange(cards); }));
      box.appendChild(textInput(⟦json.dumps(_t('Button URL'))⟧, card.actions[0].url, function(v) { card.actions[0].url = v; onValueChange(cards); }));
      box.appendChild(button('\u2715 ' + ⟦json.dumps(_t('Remove card'))⟧, function() {
        var next = cards.slice(); next.splice(i, 1);
        onStructureChange(next.length ? next : [defaultCarouselCard()]);
      }));
      wrap.appendChild(box);
    });
    wrap.appendChild(button(⟦json.dumps(_t('+ Add card'))⟧, function() { onStructureChange(cards.concat([defaultCarouselCard()])); }));
    return wrap;
  }

  function renderBlockFields(block) {
    var body = document.createElement('div');
    body.className = 'block-builder-card-body';
    if (block.type === 'header') {
      body.appendChild(textInput(⟦json.dumps(_t('Title'))⟧, block.text.text, function(v) { block.text.text = v; renderPreview(); }));
    } else if (block.type === 'markdown') {
      body.appendChild(textArea(⟦json.dumps(_t('Markdown text ({token} available)', token='{{message}}'))⟧, block.text, function(v) { block.text = v; renderPreview(); }));
    } else if (block.type === 'divider') {
      body.appendChild(note(⟦json.dumps(_t('No fields.'))⟧));
    } else if (block.type === 'image') {
      body.appendChild(textInput(⟦json.dumps(_t('Image URL'))⟧, block.image_url, function(v) { block.image_url = v; renderPreview(); }));
      body.appendChild(textInput(⟦json.dumps(_t('Alt text'))⟧, block.alt_text, function(v) { block.alt_text = v; renderPreview(); }));
    } else if (block.type === 'context') {
      body.appendChild(stringList(⟦json.dumps(_t('Context text elements'))⟧, block.elements.map(function(e) { return e.text; }),
        function(texts) { block.elements = texts.map(function(t) { return { type: 'mrkdwn', text: t }; }); renderPreview(); },
        function(texts) { block.elements = texts.map(function(t) { return { type: 'mrkdwn', text: t }; }); renderBlockList(); }));
    } else if (block.type === 'actions') {
      body.appendChild(buttonList(block.elements,
        function(elements) { block.elements = elements; renderPreview(); },
        function(elements) { block.elements = elements; renderBlockList(); }));
    } else if (block.type === 'section' && block.fields) {
      body.appendChild(stringList(⟦json.dumps(_t('Fields'))⟧, block.fields.map(function(f) { return f.text; }),
        function(texts) { block.fields = texts.map(function(t) { return { type: 'mrkdwn', text: t }; }); renderPreview(); },
        function(texts) { block.fields = texts.map(function(t) { return { type: 'mrkdwn', text: t }; }); renderBlockList(); }));
    } else if (block.type === 'section') {
      body.appendChild(textArea(⟦json.dumps(_t('Text ({token} available)', token='{{message}}'))⟧, block.text.text, function(v) { block.text.text = v; renderPreview(); }));
    } else if (block.type === 'carousel') {
      body.appendChild(cardList(block.elements,
        function(elements) { block.elements = elements; renderPreview(); },
        function(elements) { block.elements = elements; renderBlockList(); }));
    }
    return body;
  }

  function renderBlockCard(block, index) {
    var card = document.createElement('div');
    card.className = 'block-builder-card';
    var header = document.createElement('div');
    header.className = 'block-builder-card-header';
    var title = document.createElement('strong');
    title.textContent = block.type;
    header.appendChild(title);
    header.appendChild(button('\u25B2', function() { moveBlock(index, -1); }));
    header.appendChild(button('\u25BC', function() { moveBlock(index, 1); }));
    header.appendChild(button('\u2715', function() { removeBlock(index); }));
    card.appendChild(header);
    card.appendChild(renderBlockFields(block));
    return card;
  }

  function moveBlock(index, delta) {
    var target = index + delta;
    if (target < 0 || target >= state.blocks.length) return;
    var tmp = state.blocks[index];
    state.blocks[index] = state.blocks[target];
    state.blocks[target] = tmp;
    renderBlockList();
  }

  function removeBlock(index) {
    state.blocks.splice(index, 1);
    renderBlockList();
  }

  function updateFormActions(name) {
    var encoded = encodeURIComponent(name || '');
    deleteForm.action = '/notifications/block-templates/' + encoded + '/delete';
    testForm.action = '/notifications/block-templates/' + encoded + '/test';
    var isUnsavedDefault = defaultOnlyNames.indexOf(name) !== -1;
    deleteForm.querySelector('button').disabled = !name || isUnsavedDefault;
    testForm.querySelector('button').disabled = !name;
  }

  function renderPreview() {
    jsonPreview.textContent = JSON.stringify(state.blocks, null, 2);
    blocksJsonHidden.value = JSON.stringify(state.blocks);
    nameHidden.value = nameInput.value;
    notificationKeyHidden.value = notificationKeySelect.value;
    updateFormActions(originalNameHidden.value);
  }

  function renderBlockList() {
    blockList.innerHTML = '';
    state.blocks.forEach(function(block, index) {
      blockList.appendChild(renderBlockCard(block, index));
    });
    renderPreview();
  }

  function loadTemplate(name) {
    var tmpl = templates[name] || { blocks: [], notification_key: null };
    state.blocks = JSON.parse(JSON.stringify(tmpl.blocks || []));
    nameInput.value = name === '__new__' ? '' : name;
    originalNameHidden.value = name === '__new__' ? '' : name;
    notificationKeySelect.value = tmpl.notification_key || '';
    renderBlockList();
  }

  templateSelect.innerHTML = '';
  var newOption = document.createElement('option');
  newOption.value = '__new__';
  newOption.textContent = ⟦json.dumps(_t('+ New template'))⟧;
  templateSelect.appendChild(newOption);
  Object.keys(templates).forEach(function(name) {
    var option = document.createElement('option');
    option.value = name;
    option.textContent = defaultOnlyNames.indexOf(name) !== -1 ? name + ' ' + ⟦json.dumps(_t('(default)'))⟧ : name;
    templateSelect.appendChild(option);
  });

  document.querySelectorAll('[data-add-block]').forEach(function(btn) {
    btn.addEventListener('click', function() {
      state.blocks.push(defaultBlock(btn.getAttribute('data-add-block')));
      renderBlockList();
    });
  });
  templateSelect.addEventListener('change', function() { loadTemplate(templateSelect.value); });
  nameInput.addEventListener('input', renderPreview);
  notificationKeySelect.addEventListener('change', renderPreview);

  loadTemplate('__new__');
})();
</script>
</section>
