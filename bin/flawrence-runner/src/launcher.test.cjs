// Run with: node --test bin/flawrence-runner/src/launcher.test.cjs
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const script = fs.readFileSync(path.join(__dirname, 'launcher.html'), 'utf8')
  .split('<script>')[1].split('</script>')[0];

// Only the DOM operations used by the launcher; dotenv parsing is tested in Rust.
class Element {
  value = '';
  checked = false;
  disabled = false;
  children = [];
  listeners = {};
  classList = {
    add: value => { this.className = [...new Set((this.className ?? '').split(' ').filter(Boolean).concat(value))].join(' '); },
    remove: value => { this.className = (this.className ?? '').split(' ').filter(name => name !== value).join(' '); },
    contains: value => (this.className ?? '').split(' ').includes(value),
  };
  append(child) { child.parentElement = this; this.children.push(child); }
  replaceChildren(...children) { this.children = []; children.forEach(child => this.append(child)); }
  add(child) { this.append(child); }
  get options() { return this.children; }
  remove() { this.parentElement.children.splice(this.parentElement.children.indexOf(this), 1); }
  setAttribute(name, value) { this[name] = value; }
  removeAttribute(name) { delete this[name]; }
  focus() { this.focused = true; }
  scrollIntoView() {}
  addEventListener(type, listener) { this.listeners[type] = listener; }
  async emit(type, event = {}) {
    await this.listeners[type]?.({ preventDefault() {}, ...event });
    if (type === 'input') await this.parentElement?.emit(type);
  }
  set innerHTML(html) {
    this.replaceChildren();
    for (const match of html.matchAll(/<[a-z][\w-]*\b([^>]*)>/gi)) {
      if (!/\bid="/.test(match[1])) continue;
      const input = new Element();
      for (const attribute of match[1].matchAll(/([\w-]+)="([^"]*)"/g)) input[attribute[1]] = attribute[2];
      this.append(input);
    }
  }
  querySelector(selector) {
    for (const child of this.children) {
      if (selector === '#' + child.id || selector === 'input[type=range]' && child.type === 'range') return child;
      const found = child.querySelector?.(selector);
      if (found) return found;
    }
    return null;
  }
}

async function launcher(overrides = {}) {
  const ids = ['form', 'inputs', 'agent', 'env', 'effective-env', 'env-note', 'env-controls',
    'environment-section', 'interrupt', 'status', 'dashboard', 'web-ui', 'output-log', 'log-note', 'directory', 'artifacts', 'query', 'launch-effect'];
  const elements = Object.fromEntries(ids.map(id => [id, new Element()]));
  elements.submit = new Element();
  elements['launch-effect'].hidden = true;
  elements['web-ui'].checked = true;
  elements.form.querySelector = () => elements.submit;
  elements['environment-section'].append(elements['env-controls']);
  elements['environment-section'].append(elements.env);
  const defaults = {
    directory: '/workspace', artifacts: '/artifacts', env: 'CUSTOM_SETTING=default', manage_providers: true,
    control_defaults: { MAX_TOKENS: '65536', MAX_ITERATIONS: '500', FLAWRENCE_MAX_PARALLEL_AGENTS: '40',
      FLAWRENCE_PREWARM_BUILD: 'true', FLAWRENCE_WORKTREE_ISOLATION: 'false', FLAWRENCE_WEB_MAX_EVENTS: '1000' },
    ...overrides,
    controls: { FLAWRENCE_MODEL: 'custom/model', FLAWRENCE_PROVIDER_CUSTOM_BASE_URL: 'http://custom.invalid/v1', FLAWRENCE_PROVIDER_CUSTOM_API_KEY: 'test-key', ...overrides.controls },
  };
  const app = { state: { running: false, url: null }, previews: [], starts: [], openedTabs: [], timers: new Map(), timerId: 0, modelRequests: [], documentListeners: {}, elements };
  app.fireTimer = id => { const timer = app.timers.get(id); app.timers.delete(id); timer?.callback(); };
  app.finishAnimation = () => { for (const [id, timer] of app.timers) if (timer.delay === 3900) app.fireTimer(id); };
  const context = vm.createContext({
    document: { querySelector: selector => elements[selector.slice(1)], createElement: () => new Element(), addEventListener: (type, listener) => { app.documentListeners[type] = listener; } },
    Option: function(text, value) { this.text = text; this.value = value; },
    FormData: function() { return ['directory', 'artifacts', 'agent', 'query', 'env'].map(key => [key, elements[key].value]); },
    window: { open: (url, target, features) => { app.openedTabs.push({url, target, features}); return app.openWindow?.(); } },
    setInterval() {},
    setTimeout(callback, delay) {
      const id = ++app.timerId;
      app.timers.set(id, {callback, delay});
      if (delay === 3900 && !app.manualAnimation) Promise.resolve().then(() => app.fireTimer(id));
      return id;
    },
    clearTimeout(id) { app.timers.delete(id); },
    fetch: async (url, options) => {
      const body = options.body && JSON.parse(options.body);
      let result;
      switch (url) {
        case '/launcher/defaults': result = defaults; break;
        case '/launcher/status': result = app.state; break;
        case '/launcher/agents': result = ['security-review', 'custom-agent']; break;
        case '/launcher/log': result = { run_id: 0, text: '' }; break;
        case '/launcher/environment':
          app.previews.push(body);
          result = app.preview ? await app.preview(body) : {
            effective: JSON.stringify(body), editor: body,
            providers: app.providers ?? Object.keys(body.controls).filter(key => key.startsWith('FLAWRENCE_PROVIDER_') && key.endsWith('_BASE_URL')).map(key => {
              const name = key.slice('FLAWRENCE_PROVIDER_'.length, -'_BASE_URL'.length);
              return { id: name.toLowerCase(), custom: true, available: Boolean(body.controls[key] && body.controls[`FLAWRENCE_PROVIDER_${name}_API_KEY`]), problem: 'Incomplete' };
            }).concat([
              {id: 'workers-ai', custom: false, available: Boolean(body.controls.CLOUDFLARE_API_TOKEN && body.controls.CLOUDFLARE_ACCOUNT_ID)},
              {id: 'openai', custom: false, available: Boolean(body.controls.OPENAI_API_KEY || (body.controls.OPENCODE_ACCESS_TOKEN && body.controls.FLAWRENCE_OPENAI_BASE_URL))},
              {id: 'anthropic', custom: false, available: Boolean(body.controls.ANTHROPIC_API_KEY)},
            ]),
            model_error: app.modelError ?? null,
          };
          break;
        case '/launcher/models':
          app.modelRequests.push(body);
          result = app.models ? await app.models(body) : ['org/model-a', 'org/model-b'];
          break;
        case '/launcher/start':
          app.starts.push(body);
          result = app.start ? await app.start(body) : app.state = { running: true, url: '/run/1/', input: body, interrupts: 0 };
          break;
        default: throw new Error(url);
      }
      return { ok: true, json: async () => result };
    },
  });
  app.run = code => vm.runInContext(code, context);
  await app.run(script);
  app.setting = key => app.run('settings').find(setting => setting.key === key);
  app.environment = () => JSON.parse(JSON.stringify(app.run('readEnvironment()')));
  app.provider = app.run('modelProvider');
  return app;
}

test('server values populate controls, absent values use runner defaults, and optional limits are unset', async () => {
  const app = await launcher({ controls: { FLAWRENCE_MODEL: 'custom/model', MAX_TOKENS: '131072', FLAWRENCE_PREWARM_BUILD: 'off', TOKEN_COST_INPUT: '0.25' } });
  assert.equal(app.setting('MAX_TOKENS').field.value, '131072');
  assert.equal(app.setting('FLAWRENCE_MAX_PARALLEL_AGENTS').field.value, '40');
  assert.equal(app.setting('MAX_ITERATIONS').field.value, '500');
  assert.equal(app.setting('FLAWRENCE_PREWARM_BUILD').field.checked, false);
  assert.equal(app.setting('FLAWRENCE_WORKTREE_ISOLATION').field.checked, false);
  assert.equal(app.setting('FLAWRENCE_AGENT_MEMORY_MB').field.value, 'No Memory Cap');
  assert.equal(app.setting('MAX_CONTEXT_TOKENS').field.value, 'No Context Cap');
  assert.equal(app.setting('FLAWRENCE_WEB_MAX_EVENTS').field.value, '1000');
  assert.equal(app.environment().controls.FLAWRENCE_WEB_MAX_EVENTS, '1000');
  assert.equal(app.setting('TOKEN_COST_INPUT').field.value, '0.25');
  assert.equal(app.elements.env.value, 'CUSTOM_SETTING=default');
  assert.equal(app.elements['effective-env'].value, JSON.stringify(app.environment()));
  assert.equal(app.elements.submit.disabled, false);
});

test('model loads into its dedicated input and clearing it omits the variable', async () => {
  const app = await launcher();
  const model = app.setting('FLAWRENCE_MODEL');
  assert.equal(model.field.type, 'text');
  assert.equal(model.field.value, 'model');
  assert.equal(app.provider.value, 'custom');
  assert.equal(app.environment().controls.FLAWRENCE_MODEL, 'custom/model');
  assert.equal(app.elements.env.value.includes('FLAWRENCE_MODEL'), false);
  const controls = app.elements['env-controls'].children;
  const position = controls.indexOf(model.field.parentElement);
  assert.equal(controls[position].className, 'env-control model');
  assert.equal(controls[position + 1], app.setting('TOKEN_COST_INPUT').field.parentElement);
  model.field.value = '';
  await model.field.emit('input');
  await app.run('updateEnvironmentPreview()');
  assert.equal('FLAWRENCE_MODEL' in app.environment().controls, false);
  model.field.value = '  ';
  assert.equal('FLAWRENCE_MODEL' in app.environment().controls, false);
});

test('unrepresentable server values remain exact textarea overrides', async () => {
  const app = await launcher({ controls: { MAX_TOKENS: '999999', MAX_ITERATIONS: '25', TOKEN_COST_INPUT: 'NaN' } });
  assert.equal(app.setting('MAX_TOKENS').field.value, '65536');
  assert.match(app.elements.env.value, /MAX_TOKENS="999999"/);
  assert.match(app.elements.env.value, /MAX_ITERATIONS="25"/);
  assert.match(app.elements.env.value, /TOKEN_COST_INPUT="NaN"/);
  assert.equal(app.run('envLine("CUSTOM", "a\\n$HOME\\\\quote\\\"")'), 'CUSTOM="a\\n\\$HOME\\\\quote\\\""\n');
});

test('sliders, number inputs, checkboxes and optional prices update preview payloads', async () => {
  const app = await launcher();
  const memory = app.setting('FLAWRENCE_AGENT_MEMORY_MB');
  memory.range.value = '2048';
  await memory.range.emit('input');
  assert.equal(memory.field.value, '2048');
  memory.field.value = '4096';
  await memory.field.emit('input');
  assert.equal(memory.range.value, '4096');
  app.setting('TOKEN_COST_INPUT').field.value = '1.25';
  app.setting('TOKEN_COST_OUTPUT').field.value = '5.5';
  app.elements.env.value = 'MAX_TOKENS=999999\nCUSTOM=yes';
  await app.run('updateEnvironmentPreview()');
  assert.equal(app.environment().controls.FLAWRENCE_AGENT_MEMORY_MB, '4096');
  assert.equal(app.environment().controls.TOKEN_COST_INPUT, '1.25');
  assert.equal(app.environment().env, app.elements.env.value);
  memory.range.value = '32769';
  await memory.range.emit('input');
  await app.run('updateEnvironmentPreview()');
  assert.equal(memory.field.value, 'No Memory Cap');
  assert.equal(memory.field.readOnly, true);
  assert.equal('FLAWRENCE_AGENT_MEMORY_MB' in app.environment().controls, false);
  assert.equal('TOKEN_COST_CACHE_READ' in app.environment().controls, false);
});

test('only the slider position beyond each numeric maximum unsets the cap', async () => {
  const app = await launcher();
  for (const [key, maximum, label] of [
    ['FLAWRENCE_AGENT_MEMORY_MB', 32768, 'No Memory Cap'],
    ['MAX_CONTEXT_TOKENS', 1048576, 'No Context Cap'],
  ]) {
    const { range, field } = app.setting(key);
    range.value = String(maximum);
    await range.emit('input');
    assert.equal(field.type, 'number');
    assert.equal(field.readOnly, false);
    assert.equal(app.environment().controls[key], String(maximum));
    range.value = String(maximum + 1);
    await range.emit('input');
    assert.equal(field.value, label);
    assert.equal(range['aria-valuetext'], label);
    assert.equal(key in app.environment().controls, false);
    range.value = String(maximum);
    await range.emit('input');
    assert.equal(field.type, 'number');
    assert.equal(field.readOnly, false);
    assert.equal(app.environment().controls[key], String(maximum));
  }
});

test('active runs restore controls, raw overrides and preview; later polling preserves drafts', async () => {
  const app = await launcher();
  const input = { directory: '/active', artifacts: '/active-artifacts', agent: 'custom-agent', query: 'original query',
    env: 'MAX_TOKENS=999999\nFLAWRENCE_MODEL=override/model', manage_providers: true, controls: { FLAWRENCE_MODEL: 'active/model', MAX_TOKENS: '131072', FLAWRENCE_AGENT_MEMORY_MB: '1024' } };
  app.state = { running: true, url: '/run/7/', input, interrupts: 0 };
  await app.run('poll()');
  assert.equal(app.elements.inputs.disabled, true);
  assert.equal(app.elements.interrupt.disabled, false);
  assert.equal(app.setting('FLAWRENCE_AGENT_MEMORY_MB').field.value, '1024');
  assert.equal(app.setting('MAX_TOKENS').field.value, '131072');
  assert.equal(app.elements.env.value, input.env);
  assert.equal(app.setting('FLAWRENCE_MODEL').field.value, 'model');
  assert.equal(app.provider.value, 'active');
  assert.equal(app.elements.query.value, input.query);
  assert.equal(app.elements['effective-env'].value, JSON.stringify(app.environment()));
  app.state.running = false;
  await app.run('poll()');
  app.elements.env.value = 'NEXT=draft';
  app.setting('MAX_TOKENS').field.value = '262144';
  await app.run('poll()');
  assert.equal(app.elements.inputs.disabled, false);
  assert.equal(app.elements.env.value, 'NEXT=draft');
  assert.equal(app.setting('MAX_TOKENS').field.value, '262144');
});

test('stale previews cannot overwrite newer edits and parse errors prevent submission', async () => {
  const app = await launcher();
  const pending = [];
  app.preview = () => new Promise(resolve => pending.push(resolve));
  const first = app.run('updateEnvironmentPreview()');
  const second = app.run('updateEnvironmentPreview()');
  pending[1]({ effective: 'NEW=value' });
  await second;
  pending[0]({ effective: 'OLD=value' });
  await first;
  assert.equal(app.elements['effective-env'].value, 'NEW=value');
  app.preview = () => { throw new Error('Invalid environment'); };
  await app.run('updateEnvironmentPreview()');
  assert.equal(app.elements['effective-env'].value, '');
  assert.equal(app.elements['env-note'].textContent, 'Invalid environment');
  assert.equal(app.elements.submit.disabled, true);
});

test('submission includes both editor layers and restored launch state matches the payload', async () => {
  const app = await launcher();
  app.elements.env.value = 'MAX_TOKENS=999999';
  app.setting('FLAWRENCE_PREWARM_BUILD').field.checked = false;
  const expected = app.environment();
  await app.elements.form.emit('submit');
  assert.equal(app.starts[0].env, expected.env);
  assert.deepEqual(app.starts[0].controls, expected.controls);
  assert.equal(app.elements.inputs.disabled, true);
  assert.equal(app.elements['effective-env'].value, JSON.stringify(expected));
});

test('Start sends the run immediately but opens only the clean dashboard after the launch animation', async () => {
  const app = await launcher();
  app.manualAnimation = true;
  const submission = app.elements.form.emit('submit');
  assert.equal(app.starts.length, 1);
  assert.equal(app.starts[0].web_ui, true);
  assert.equal(app.openedTabs.length, 0);
  assert.equal(app.elements['launch-effect'].hidden, false);
  assert.equal(app.elements.form.classList.contains('launching'), true);
  await app.elements.form.emit('submit');
  assert.equal(app.starts.length, 1);
  app.finishAnimation();
  await submission;
  assert.deepEqual(app.openedTabs, [{url: '/run/1/', target: '_blank', features: 'noopener'}]);
  assert.equal(app.elements['launch-effect'].hidden, true);
  assert.equal(app.elements.form.classList.contains('launching'), false);
});

test('a slow start response still keeps the dashboard closed after the animation until the run is accepted', async () => {
  const app = await launcher();
  app.manualAnimation = true;
  let accept;
  app.start = () => new Promise(resolve => { accept = resolve; });
  const submission = app.elements.form.emit('submit');
  app.finishAnimation();
  assert.equal(app.openedTabs.length, 0);
  assert.equal(app.elements['launch-effect'].hidden, true);
  accept({running: true, url: '/run/2/', interrupts: 0});
  await submission;
  assert.equal(app.openedTabs[0].url, '/run/2/');
});

test('disabling the dashboard submits false, skips the popup and restores the checkbox on polling', async () => {
  const app = await launcher();
  app.elements['web-ui'].checked = false;
  await app.elements.form.emit('submit');
  assert.equal(app.starts[0].web_ui, false);
  assert.equal(app.openedTabs.length, 0);
  assert.equal(app.elements.dashboard.hidden, true);
  assert.match(app.elements.status.textContent, /Dashboard disabled/);
  app.run('restoredRunUrl = null');
  app.elements['web-ui'].checked = true;
  await app.run('poll()');
  assert.equal(app.elements['web-ui'].checked, false);
  app.state.running = false;
  await app.run('poll()');
  assert.equal(app.elements.inputs.disabled, false);
  app.elements['web-ui'].checked = true;
  await app.elements.form.emit('submit');
  assert.equal(app.starts[1].web_ui, true);
  assert.equal(app.openedTabs.length, 1);
  assert.equal(app.elements.dashboard.hidden, false);
});

test('rejected starts clear the effect and timers without opening a tab, and retries work', async () => {
  const app = await launcher();
  app.manualAnimation = true;
  app.start = () => { throw new Error('Start rejected'); };
  await app.elements.form.emit('submit');
  assert.equal(app.openedTabs.length, 0);
  assert.equal(app.elements['launch-effect'].hidden, true);
  assert.equal(app.elements.form.classList.contains('launching'), false);
  assert.equal([...app.timers.values()].some(timer => timer.delay === 3900), false);
  assert.equal(app.elements.status.textContent, 'Start rejected');
  app.start = null;
  const retry = app.elements.form.emit('submit');
  assert.equal(app.starts.length, 2);
  app.finishAnimation();
  await retry;
  assert.equal(app.openedTabs.length, 1);
});

test('a blocked delayed popup leaves the running dashboard link available', async () => {
  const app = await launcher();
  app.openWindow = () => null;
  await app.elements.form.emit('submit');
  assert.equal(app.elements.dashboard.hidden, false);
  assert.equal(app.elements.dashboard.href, '/run/1/');
  assert.equal(app.elements.inputs.disabled, true);
});

test('multiple provider rows reconstruct pairs and removal clears their submitted settings', async () => {
  const app = await launcher();
  const row = app.run('addCustomProvider("second", "https://second.invalid/v1", "second-key")');
  await app.run('updateEnvironmentPreview()');
  assert.equal(app.environment().controls.FLAWRENCE_PROVIDER_SECOND_BASE_URL, 'https://second.invalid/v1');
  assert.equal(app.environment().controls.FLAWRENCE_PROVIDER_SECOND_API_KEY, 'second-key');
  assert.equal(row.key.type, 'password');
  assert.equal(row.status.textContent, 'Available');
  assert.deepEqual(app.provider.options.map(option => option.value), ['', 'custom', 'second']);
  await row.element.children.find(child => child.id?.endsWith('-remove')).emit('click');
  await app.run('updateEnvironmentPreview()');
  assert.equal('FLAWRENCE_PROVIDER_SECOND_API_KEY' in app.environment().controls, false);
  assert.equal(app.provider.options.some(option => option.value === 'second'), false);
  assert.equal(app.provider.value, 'custom');
});

test('provider visibility does not change settings and duplicate or reserved identifiers block Start', async () => {
  const app = await launcher();
  const before = app.environment();
  await app.run('providersToggle.checked = true; providersToggle.listeners.change()');
  assert.equal(app.run('providerPanels.hidden'), false);
  await app.run('providersToggle.checked = false; providersToggle.listeners.change()');
  assert.deepEqual(app.environment(), before);
  app.run('addCustomProvider("CUSTOM", "https://duplicate.invalid/v1", "key")');
  await app.run('updateEnvironmentPreview()');
  assert.equal(app.elements.submit.disabled, true);
  assert.match(app.elements['env-note'].textContent, /Duplicate/);
  app.run('customProviders[1].identifier.value = "openai"');
  await app.run('updateEnvironmentPreview()');
  assert.equal(app.elements.submit.disabled, true);
  assert.match(app.elements['env-note'].textContent, /built-in names/);
});

test('no model is prefilled and merged server availability controls the dropdown and launch validation', async () => {
  const app = await launcher({controls: { FLAWRENCE_MODEL: '' }});
  assert.equal(app.provider.value, '');
  assert.equal(app.setting('FLAWRENCE_MODEL').field.value, '');
  assert.equal(app.modelRequests.length, 0);
  app.providers = [{id: 'fromtextarea', custom: true, available: true}];
  await app.run('updateEnvironmentPreview()');
  assert.deepEqual(app.provider.options.map(option => option.value), ['', 'fromtextarea']);
  app.providers = [];
  app.modelError = 'Configure at least one available provider before starting a run';
  await app.run('updateEnvironmentPreview()');
  assert.equal(app.elements.submit.disabled, true);
  app.providers = [{id: 'anthropic', custom: false, available: true}];
  app.modelError = null;
  await app.run('updateEnvironmentPreview()');
  assert.equal(app.elements.submit.disabled, false);
  assert.equal('FLAWRENCE_MODEL' in app.environment().controls, false);
});

test('built-in credentials restore with optional endpoints and are masked', async () => {
  const app = await launcher({controls: {
    FLAWRENCE_MODEL: 'workers-ai/@cf/org/model', CLOUDFLARE_API_TOKEN: 'cf-key', CLOUDFLARE_ACCOUNT_ID: 'account',
    FLAWRENCE_WORKERS_AI_BASE_URL: 'https://worker.invalid/v1', OPENAI_API_KEY: 'openai-key', OPENCODE_ACCESS_TOKEN: 'gateway-key', ANTHROPIC_API_KEY: 'anthropic-key',
  }});
  assert.equal(app.provider.value, 'workers-ai');
  assert.equal(app.setting('FLAWRENCE_MODEL').field.value, '@cf/org/model');
  assert.equal(app.environment().controls.FLAWRENCE_MODEL, 'workers-ai/@cf/org/model');
  for (const key of ['CLOUDFLARE_API_TOKEN', 'OPENAI_API_KEY', 'OPENCODE_ACCESS_TOKEN', 'ANTHROPIC_API_KEY']) {
    assert.equal(app.run(`providerFields.get('${key}').type`), 'password');
  }
  app.run('providerFields.get("FLAWRENCE_WORKERS_AI_BASE_URL").value = ""');
  assert.equal('FLAWRENCE_WORKERS_AI_BASE_URL' in app.environment().controls, false);
});

test('model discovery never replaces typed IDs and stale results or failures do not block manual entry', async () => {
  const app = await launcher();
  const pending = [];
  app.models = () => new Promise(resolve => pending.push(resolve));
  const first = app.run('refreshModelSuggestions(true)');
  app.setting('FLAWRENCE_MODEL').field.value = 'manually/typed-id';
  const second = app.run('refreshModelSuggestions(true)');
  pending[1](['org/new-model']);
  await second;
  pending[0](['org/stale-model']);
  await first;
  assert.equal(app.setting('FLAWRENCE_MODEL').field.value, 'manually/typed-id');
  assert.deepEqual(Array.from(app.run('modelSuggestions.children'), option => option.textContent), ['org/new-model']);
  app.models = () => { throw new Error('Models endpoint unsupported; enter an ID manually'); };
  await app.run('refreshModelSuggestions(true)');
  assert.match(app.run('modelNote.textContent'), /unsupported/);
  assert.equal(app.elements.submit.disabled, false);
  assert.equal(app.environment().controls.FLAWRENCE_MODEL, 'custom/manually/typed-id');
});

test('the in-page model picker opens the full list with a populated input and clicking updates the override', async () => {
  const app = await launcher();
  await app.run('refreshModelSuggestions(true)');
  const picker = app.run('modelSuggestions');
  const toggle = app.run('toggleModels');
  assert.equal(toggle.disabled, false);
  assert.equal(picker.hidden, true);
  assert.deepEqual(picker.children.map(option => option.textContent), ['org/model-a', 'org/model-b']);
  app.setting('FLAWRENCE_MODEL').field.value = 'org/model-a';
  await app.setting('FLAWRENCE_MODEL').field.emit('input');
  await toggle.emit('click');
  assert.equal(picker.hidden, false);
  assert.equal(picker.children.length, 2);
  await picker.children[1].emit('click');
  assert.equal(app.setting('FLAWRENCE_MODEL').field.value, 'org/model-b');
  assert.equal(app.environment().controls.FLAWRENCE_MODEL, 'custom/org/model-b');
  assert.equal(picker.hidden, true);
  app.run('clearModelSuggestions()');
  assert.equal(toggle.disabled, true);
  assert.equal(app.setting('FLAWRENCE_MODEL').field.value, 'org/model-b');
});

test('model picker keyboard selection, escape and outside clicks work without changing manual text', async () => {
  const app = await launcher();
  await app.run('refreshModelSuggestions(true)');
  const field = app.setting('FLAWRENCE_MODEL').field;
  const list = app.run('modelSuggestions');
  await field.emit('keydown', {key: 'ArrowDown'});
  assert.equal(list.hidden, false);
  assert.equal(field['aria-expanded'], 'true');
  assert.equal(field.value, 'model');
  await field.emit('keydown', {key: 'ArrowDown'});
  await field.emit('keydown', {key: 'Enter'});
  assert.equal(field.value, 'org/model-b');
  assert.equal(list.hidden, true);
  await field.emit('keydown', {key: 'ArrowUp'});
  await field.emit('keydown', {key: 'Escape'});
  assert.equal(list.hidden, true);
  assert.equal(field.value, 'org/model-b');
  await app.run('toggleModels').emit('click');
  app.documentListeners.mousedown({target: {closest: () => null}});
  assert.equal(list.hidden, true);
  assert.equal(field.value, 'org/model-b');
});
