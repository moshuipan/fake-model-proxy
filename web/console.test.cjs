/** 目录展示回归：未知、免费、活动到期及延迟定时器不能混淆。 */
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { test } = require('node:test');
const vm = require('node:vm');
const { webcrypto } = require('node:crypto');

/** 为通用配置辅助入口提供表单与组件，不使用真实客户端或网络。 */
function configHelperFixture() {
  const result = modelFixture();
  result.context.toasts = [];
  vm.runInContext(`
    state.plugins.find(plugin => plugin.id === 'trae_personal').ui = {config_helpers:[
      {id:'client_device',option:'client_device',capture_path:'/capture',validate_path:'/validate'}
    ]};
    toast = message => globalThis.toasts.push(message);
    renderPluginFields = () => {};
  `, result.context);
  result.nodes.set('f-kind', { value: 'trae_personal' });
  result.nodes.set('drawer', { open: true });
  result.nodes.set('f-options', { value: '{"region":"cn","routing":{"default_output_tokens":4096}}' });
  result.nodes.set('backend-form', { expectedRevision: 7, serviceSpec: {credentials:{api_key:{secret_ref:'store:kept'}}} });
  result.nodes.set('config-helper-0', {});
  result.nodes.set('config-candidates-0', { hidden: true });
  result.nodes.set('config-status-0', { textContent: '' });
  result.nodes.set('config-choice-0', { value: '0', innerHTML: '' });
  return result;
}

/** 单候选自动填入，保留当前标量编辑、预算、秘密引用和 CAS 版本。 */
test('device capture only fills current form and preserves other config', async () => {
  const { context, nodes } = configHelperFixture();
  nodes.set('plugin-fields', { querySelectorAll: () => [{dataset:{pluginOption:'region',type:'string'},tagName:'INPUT',value:'cn'}] });
  vm.runInContext("api = async () => ({candidates:[{label:'local',value:{device_id:'123',app_version:'3.4.24'}}]})", context);
  await vm.runInContext('captureConfigHelper(0)', context);
  const options = JSON.parse(nodes.get('f-options').value);
  assert.deepEqual(options, {region:'cn',routing:{default_output_tokens:4096},client_device:{device_id:'123',app_version:'3.4.24'}});
  assert.equal(nodes.get('backend-form').expectedRevision, 7);
  assert.equal(nodes.get('backend-form').serviceSpec.credentials.api_key.secret_ref, 'store:kept');
});

/** 多客户端必须先选择；导入报告标签转义，不把候选顺序当设备归属。 */
test('device import stages multiple candidates and applies selected one', async () => {
  const { context, nodes } = configHelperFixture();
  nodes.set('config-json-0', { value: '{"api_version":"fmp/client-device/v1"}' });
  vm.runInContext(`api = async () => ({candidates:[
    {label:'<script>bad</script>',value:{device_id:'111'}},{label:'second',value:{device_id:'222'}}
  ],warnings:[]})`, context);
  await vm.runInContext('importConfigHelper(0)', context);
  assert.equal(JSON.parse(nodes.get('f-options').value).client_device, undefined);
  assert.equal(nodes.get('config-candidates-0').hidden, false);
  assert.match(nodes.get('config-choice-0').innerHTML, /&lt;script&gt;/);
  nodes.get('config-choice-0').value = '1';
  vm.runInContext('useConfigHelperCandidate(0)', context);
  assert.equal(JSON.parse(nodes.get('f-options').value).client_device.device_id, '222');
});

/** 切换服务、替换表单或发起后续导入后，旧采集响应不能覆盖当前编辑。 */
test('late device response cannot overwrite another form or newer request', async () => {
  for (const change of [
    "document.getElementById('f-kind').value = 'qoder'",
    "globalThis.replaceForm()",
    "document.getElementById('drawer').open = false",
    "globalThis.newer = captureConfigHelper(0)",
  ]) {
    const { context, nodes } = configHelperFixture();
    context.replaceForm = () => nodes.set('backend-form', {});
    context.pending = [];
    vm.runInContext("api = () => new Promise(resolve => globalThis.pending.push(resolve)); globalThis.first = captureConfigHelper(0)", context);
    vm.runInContext(change, context);
    context.pending[0]({candidates:[{label:'old',value:{device_id:'111'}}]});
    await context.first;
    assert.equal(JSON.parse(nodes.get('f-options').value).client_device, undefined);
    if (context.newer) {
      context.pending[1]({candidates:[{label:'new',value:{device_id:'222'}}]});
      await context.newer;
      assert.equal(JSON.parse(nodes.get('f-options').value).client_device.device_id, '222');
    }
  }
});

/** 文件读取后调用服务端校验，缺注册信息和非法输入均保留原配置。 */
test('device file import validates remotely and errors leave options intact', async () => {
  const { context, nodes } = configHelperFixture();
  const original = nodes.get('f-options').value;
  context.file = { size: 12, text: async () => '{"device_id":"123"}' };
  vm.runInContext("api = async (path,request) => { globalThis.validated = {path,body:request.body}; return {candidates:[],warnings:['缺少 device_id']}; }", context);
  await assert.rejects(vm.runInContext('importConfigHelper(0,globalThis.file)', context), /缺少 device_id/);
  assert.equal(context.validated.path, '/validate');
  assert.equal(context.validated.body.device_id, '123');
  assert.equal(nodes.get('f-options').value, original);
  nodes.set('config-json-0', { value: 'invalid' });
  await assert.rejects(vm.runInContext('importConfigHelper(0)', context), /合法 JSON/);
  assert.equal(nodes.get('f-options').value, original);
});

/** 对外只有一个 Auto 入口，旧分散开关不能成为付费后备授权。 */
test('auto selector is unified and paid fallback stays explicit', () => {
  const { context } = fixture();
  const options = vm.runInContext('autoOptions()', context);
  assert.equal((options.match(/<option /g) || []).length, 1);
  assert.match(options, /value="auto"/);
  assert.doesNotMatch(options, /auto:/);
  const html = readFileSync('web/index.html', 'utf8');
  assert.match(html, /id="gateway-paid-fallback" type="checkbox"/);
  assert.doesNotMatch(html, /id="gateway-spread"/);
});

/** 提供模型列表、折叠偏好和表单字段的轻量 DOM，不触发上游请求。 */
function modelFixture() {
  const result = fixture();
  const nodes = new Map();
  result.context.document = { getElementById: (id) => {
    if (!nodes.has(id)) nodes.set(id, { value: '', textContent: '', innerHTML: '', querySelectorAll: () => [],
      showModal() { this.open = true; }, close() { this.open = false; } });
    return nodes.get(id);
  } };
  const storage = new Map();
  result.context.localStorage = { getItem: (key) => storage.get(key) || null, setItem: (key, value) => storage.set(key, value) };
  result.context.window = { confirm: () => false };
  vm.runInContext(`
    state.backends = [
      {id:'b1',name:'WorkBuddy',kind:'workbuddy',enabled:true,models:'raw',model_catalog:[{id:'raw',display_name:'GLM-5.3'}]},
      {id:'b2',name:'Qoder',kind:'qoder',enabled:false,models:'raw',model_catalog:[{id:'raw',display_name:'GLM-5.3'}]}
    ];
    state.aliases = [
      {alias:'workbuddy-glm-5.3',backend_id:'b1',model_id:'raw',is_primary:true},
      {alias:'legacy-glm',backend_id:'b1',model_id:'raw',is_primary:false},
      {alias:'qoder-glm-5.3',backend_id:'b2',model_id:'raw',is_primary:true}
    ];
    renderSelectedLimits = () => {};
  `, result.context);
  return { ...result, nodes, storage };
}

/** 公开调用值与显示名分离，历史名称和原始地址仍能找到同一模型。 */
test('model config uses persistent aliases and resolves historical names', () => {
  const { context } = modelFixture();
  for (const value of ['b1/raw', 'legacy-glm', 'workbuddy-glm-5.3']) {
    assert.equal(vm.runInContext(`modelConfigReference('${value}').model`, context), 'workbuddy-glm-5.3');
    assert.equal(vm.runInContext(`modelLabel('${value}')`, context), 'WorkBuddy / GLM-5.3');
  }
});

/** 搜索临时展开匹配组，清除搜索恢复用户偏好；停用模型仍有参数入口。 */
test('model groups restore collapse preference after search', () => {
  const { context, nodes } = modelFixture();
  vm.runInContext("saveModelGroup('b1',false); renderModels()", context);
  assert.match(nodes.get('model-list').innerHTML, /data-backend="b1" ><summary>/);
  assert.match(nodes.get('model-list').innerHTML, /data-action="model-detail" data-id="qoder-glm-5.3"/);
  assert.match(nodes.get('model-list').innerHTML, /data-id="qoder-glm-5.3" disabled/);
  nodes.get('model-search').value = 'workbuddy-glm-5.3';
  vm.runInContext('renderModels()', context);
  assert.equal(nodes.get('models-count').textContent, '1 / 2');
  assert.match(nodes.get('model-list').innerHTML, /data-backend="b1" open/);
  assert.doesNotMatch(nodes.get('model-list').innerHTML, /data-backend="b2"/);
  nodes.get('model-search').value = '';
  vm.runInContext('renderModels()', context);
  assert.match(nodes.get('model-list').innerHTML, /data-backend="b1" ><summary>/);
});

/** 专用地址、示例与复制来源一致，避免配置仍指向通用入口。 */
test('client integration examples use the selected plugin endpoint', () => {
  const { context, nodes } = modelFixture();
  nodes.set('example-client', { value: 'trae' });
  nodes.set('example-protocol', { value: 'messages' });
  nodes.set('example-model', { value: 'auto' });
  context.location = { origin: 'http://127.0.0.1:8317' };
  vm.runInContext('renderExample()', context);
  assert.equal(nodes.get('settings-endpoint').textContent, 'http://127.0.0.1:8317/clients/trae/v1');
  assert.match(nodes.get('example-code').textContent, /\/clients\/trae\/v1\/messages/);
  assert.match(nodes.get('client-session-hint').textContent, /没有读取 TRAE 原生 SessionID/);
  nodes.get('example-client').value = 'minimax_code';
  vm.runInContext('renderExample()', context);
  assert.match(nodes.get('settings-endpoint').textContent, /\/clients\/minimax-code\/v1$/);
});

/** 分组用完整键匹配，界面不能把推断组或歧义组标为原生会话。 */
test('record groups distinguish native sessions and inference evidence', () => {
  const { context, nodes } = modelFixture();
  context.location = { hash: '' };
  vm.runInContext(`
    state.records = [
      {id:'req-a',protocol:'chat',session_id:'trae:inferred_a',client:'trae',
       session:{confidence:'inferred',source:'history_ambiguous'},outcome:'ok'},
      {id:'req-b',protocol:'chat',session_id:'trae:inferred_ab',client:'trae',
       session:{confidence:'inferred',source:'history_new'},outcome:'ok'},
      {id:'req-c',protocol:'chat',session_id:'minimax_code:native',client:'minimax_code',
       session:{confidence:'explicit',source:'mavis_header'},outcome:'ok'}
    ];
    requestGroup('session_id','trae:inferred_a');
  `, context);
  assert.equal(nodes.get('requests-result').textContent, '1 / 3 条记录');
  assert.match(nodes.get('record-group').innerHTML, /TRAE · 推断会话/);
  assert.match(nodes.get('requests-table').innerHTML, /历史匹配多个会话/);
  assert.doesNotMatch(nodes.get('requests-table').innerHTML, /req-b/);
  assert.match(vm.runInContext('recordSessionTitle(state.records[2])', context), /原生会话/);
  assert.match(vm.runInContext('recordSessionDetails(state.records[0])', context), /不代表客户端原生 SessionID/);
  assert.match(vm.runInContext('recordSessionDetails(state.records[0])', context), /会话调度绑定/);
});

/** 详情显示匹配规模和 ID 归一依据，并能打开保留范围内的前一请求。 */
test('session details expose matching evidence and predecessor navigation', () => {
  const { context } = modelFixture();
  vm.runInContext(`
    state.records = [{id:'req-before'}, {id:'req-now',session_id:'trae:inferred_test',
      client:'trae',session:{confidence:'inferred',source:'history_continuation',
        previous_request_id:'req-before',evidence:{logical_messages:3,matched_messages:2,
          candidate_groups:1,normalized_tool_ids:1,matched_tool_calls:0}}}];
  `, context);
  const details = vm.runInContext('recordSessionDetails(state.records[1])', context);
  assert.match(details, /逻辑历史 3 条 · 匹配 2 条 · 候选会话 1 个/);
  assert.match(details, /已归一 1 个 TRAE 工具调用 ID/);
  assert.match(details, /data-action="request-detail" data-id="req-before"/);
  vm.runInContext('state.records.shift()', context);
  assert.doesNotMatch(vm.runInContext('recordSessionDetails(state.records[0])', context), /查看匹配的前一请求/);
});

/** 分类覆盖所有账号适配器，官方魔搭接口仍属于模型网关。 */
test('provider presets use one category per adapter and one custom gateway', () => {
  const { context } = modelFixture();
  vm.runInContext(`
    state.presets = ['openai_compat','modelscope','qoder','workbuddy','autoglm','minimax_code','monkeycode','raccoon']
      .map(kind => ({id:kind,name:kind,kind,cost_tier:'unknown'}));
  `, context);
  const html = vm.runInContext('providerPresetOptions()', context);
  const gateway = html.slice(0, html.indexOf('</optgroup>'));
  const privateAPI = html.slice(html.indexOf('</optgroup>'));
  assert.equal((html.match(/<option /g) || []).length, 10);
  assert.match(gateway, /value="modelscope"/);
  assert.match(gateway, /value=""/);
  for (const kind of ['qoder', 'workbuddy', 'autoglm', 'minimax_code', 'monkeycode', 'raccoon']) {
    assert.match(privateAPI, new RegExp(`value="${kind}"`));
    assert.doesNotMatch(gateway, new RegExp(`value="${kind}"`));
  }
});

/** 来回切换预设和自定义时，凭证、模型及高级参数不能串入其他服务。 */
test('switching service resets dependent fields without hiding gateway settings', () => {
  const { context, nodes } = modelFixture();
  vm.runInContext(`state.presets = [
    {id:'qoder', name:'Qoder（设备登录）',kind:'qoder',base_url:'https://example.test',models_hint:'auto',priority:10,cost_tier:'subscription'},
    {id:'modelscope',name:'魔搭',kind:'modelscope',base_url:'https://api-inference.modelscope.cn/v1',models_hint:'',priority:40,cost_tier:'free'}
  ]`, context);
  const settings = { hidden: false, innerHTML: 'gateway settings' };
  nodes.set('gateway-fields', settings);
  for (const id of ['qoder', 'modelscope', '']) {
    for (const field of ['f-key', 'f-options', 'f-capabilities', 'f-models']) nodes.set(field, { value: 'previous-service-data' });
    nodes.set('f-qps', { value: '77' });
    vm.runInContext(`applyPreset('${id}')`, context);
    assert.equal(nodes.get('f-qps').value, 0);
    for (const field of ['f-key', 'f-options', 'f-capabilities']) assert.equal(nodes.get(field).value, '');
    assert.equal(nodes.get('provider-gateway-fields').hidden, id === 'qoder');
    assert.equal(nodes.get('private-fields').hidden, id !== 'qoder');
    assert.equal(settings.hidden, false);
    assert.equal(settings.innerHTML, 'gateway settings');
  }
  assert.equal(nodes.get('f-kind').value, 'openai_compat');
  assert.equal(nodes.get('f-url').value, '');
  assert.equal(nodes.get('f-models').value, '');
  assert.equal(nodes.get('f-key').required, false);
});

/** 限频写入宿主 policy，保存保留凭证引用、区域和其余路由预算。 */
test('service QPS saves in policy while preserving plugin options and secrets', async () => {
  const { context, nodes } = modelFixture();
  const fields = { 'f-id':'service', 'f-name':'Service', 'f-kind':'trae_personal',
    'f-url':'https://trae-api-cn.mchost.guru', 'f-models':'model', 'f-key':'',
    'f-priority':'40', 'f-cost':'subscription', 'f-budget':'0', 'f-enabled':'false', 'f-qps':'3',
    'f-options':'{"region":"cn","routing":{"default_output_tokens":2048}}', 'f-capabilities':'{}' };
  for (const [id, value] of Object.entries(fields)) nodes.set(id, { value });
  nodes.set('backend-form', { expectedRevision: 5,
    serviceSpec:{policy:{image_tokens:1024,provider_input_reserve:512,qps:9},
      credentials:{api_key:{secret_ref:'store:kept'}}} });
  context.saved = null;
  vm.runInContext(`
    providerPlugin = () => ({id:'trae_personal',config_schema_version:1});
    previewChange = async (change, callback, revision) => { globalThis.saved = {change,revision}; };
  `, context);
  await vm.runInContext('saveProvider()', context);
  const saved = JSON.parse(JSON.stringify(context.saved));
  assert.equal(saved.revision, 5);
  const service = saved.change.services[0];
  assert.deepEqual(service.config.options, {region:'cn'});
  assert.equal(service.policy.qps, 3);
  assert.equal(service.policy.default_output_tokens, 2048);
  assert.equal(service.policy.image_tokens, 1024);
  assert.equal(service.policy.provider_input_reserve, 512);
  assert.equal(service.credentials.api_key.secret_ref, 'store:kept');
});

/** 新增和轮换密钥只保存配置契约允许的引用，重试不再次写入已缓存的秘密。 */
test('new service keys map the write response to a valid cached secret reference', async () => {
  for (const existing of [false, true]) {
    const { context, nodes } = modelFixture();
    const fields = { 'f-id':'service', 'f-name':'Service', 'f-kind':'openai_compat',
      'f-url':'http://127.0.0.1:1/v1', 'f-models':'model', 'f-key':'synthetic-new-key',
      'f-priority':'40', 'f-cost':'unknown', 'f-budget':'0', 'f-enabled':'false', 'f-qps':'0',
      'f-options':'{}', 'f-capabilities':'{}' };
    for (const [id, value] of Object.entries(fields)) nodes.set(id, { value });
    nodes.set('backend-form', { expectedRevision: 5, serviceSpec: existing
      ? { credentials: { api_key: { secret_ref:'store:old' }, headers:{'x-key':{secret_ref:'store:header'}} } }
      : undefined });
    context.writes = [];
    context.changes = [];
    vm.runInContext(`
      providerPlugin = () => ({id:'openai_compat',config_schema_version:1});
      api = async (path, request) => {
        globalThis.writes.push({path,request});
        return {secret_ref:'store:new',configured:true};
      };
      previewChange = async (change) => { globalThis.changes.push(change); };
    `, context);
    await vm.runInContext('saveProvider()', context);
    await vm.runInContext('saveProvider()', context);
    assert.equal(context.writes.length, 1);
    assert.equal(context.writes[0].path, '/api/v2/secrets');
    assert.equal(nodes.get('f-key').value, '');
    for (const change of JSON.parse(JSON.stringify(context.changes))) {
      const credentials = change.services[0].credentials;
      assert.deepEqual(credentials.api_key, { secret_ref:'store:new' });
      assert.equal(JSON.stringify(change).includes('synthetic-new-key'), false);
      if (existing) assert.deepEqual(credentials.headers, { 'x-key': { secret_ref:'store:header' } });
    }
    assert.deepEqual(JSON.parse(JSON.stringify(nodes.get('backend-form').secretRef)), {secret_ref:'store:new'});
  }
});

/** 关闭保护比较当前字段，恢复原值后不提示，拒绝确认时保留草稿。 */
test('dirty form protection detects values and reverted edits', () => {
  const { context } = modelFixture();
  const field = { id: 'name', tagName: 'INPUT', value: 'original' };
  const form = { elements: [field], dataset: {} };
  context.testForm = form;
  context.testDialog = { open: true, querySelector: () => form };
  vm.runInContext('trackDraft(testForm)', context);
  assert.equal(vm.runInContext('allowDiscard(testDialog)', context), true);
  field.value = 'edited';
  assert.equal(vm.runInContext('allowDiscard(testDialog)', context), false);
  assert.equal(context.document.getElementById('discard-dialog').open, true);
  field.value = 'original';
  assert.equal(vm.runInContext('allowDiscard(testDialog)', context), true);
});

/** 保存后发生的并发刷新需要再次读取，所有等待者得到最后一轮数据。 */
test('refresh coalesces concurrent reads and follows writes with a fresh read', async () => {
  const { context } = modelFixture();
  vm.runInContext(`
    globalThis.reads = 0;
    globalThis.finishRead = null;
    refreshSnapshot = async () => {
      reads++;
      await new Promise((resolve) => { finishRead = resolve; });
    };
    globalThis.first = refresh();
    globalThis.second = refresh();
  `, context);
  assert.equal(context.first, context.second);
  context.finishRead();
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(context.reads, 2);
  context.finishRead();
  await context.first;
  assert.equal(vm.runInContext('state.refreshPromise', context), null);
});

/** 隔离控制台函数，不运行初始化、不联网，并提供可控制的时钟与定时器。 */
function fixture() {
  let now = 100000;
  const timers = new Map();
  let timerId = 0;
  const context = vm.createContext({
    crypto: webcrypto,
    Date: { now: () => now },
    setTimeout: (callback, delay) => { timers.set(++timerId, { callback, delay }); return timerId; },
    clearTimeout: (id) => timers.delete(id),
  });
  const source = readFileSync(__dirname + '/console.js', 'utf8').replace(/init\(\);\s*$/, '');
  vm.runInContext(source, context);
  vm.runInContext(`state.plugins = [
    {id:'openai_compat',ui_group:'gateway'},{id:'modelscope',ui_group:'gateway'},
    ...['qoder','workbuddy','autoglm','minimax_code','monkeycode','raccoon','trae_personal'].map(id => ({id,ui_group:'reverse'}))
  ]`, context);
  return { context, timers, at: (value) => { now = value; } };
}

/** 魔搭说明只对应官方推理端点；费用数字和今日用量不能伪装成账号余额。 */
test('modelscope guidance uses the official endpoint and keeps quota unknown', () => {
  const { context } = modelFixture();
  context.URL = URL;
  for (const base_url of ['https://api-inference.modelscope.cn/v1', 'https://api-inference.modelscope.cn/v1/']) {
    context.endpoint = base_url;
    assert.equal(vm.runInContext("isModelScope({kind:'openai_compat',base_url:endpoint})", context), true);
  }
  for (const base_url of ['http://api-inference.modelscope.cn/v1', 'https://api-inference.modelscope.cn.evil.test/v1', 'https://api-inference.modelscope.cn:8443/v1', 'https://api-inference.modelscope.cn/v2']) {
    context.endpoint = base_url;
    assert.equal(vm.runInContext("isModelScope({kind:'openai_compat',base_url:endpoint})", context), false);
  }
  const card = vm.runInContext(`providerCard({id:'ms',name:'ModelScope',kind:'openai_compat',
    base_url:'https://api-inference.modelscope.cn/v1',enabled:true,has_api_key:false,cost_tier:'free'})`, context);
  assert.match(card, /待配置令牌/);
  assert.match(card, /剩余额度未知/);
  assert.doesNotMatch(card, /2000/);
  assert.match(vm.runInContext('modelScopeGuidance()', context), /https:\/\/modelscope.cn\/my\/myaccesstoken/);
});

/** 专用魔搭服务按官方精确快照展示魔粒，并继续使用 API Key 表单。 */
test('modelscope renders precise magicubes and its daily login action', () => {
  const { context } = modelFixture();
  vm.runInContext(`
    state.accounts = [{id:'ms-account',backend_id:'ms',name:'魔搭 Access Token',enabled:true,has_credential:true}];
    state.quota = [normalizeQuota({account_id:'ms-account',backend_id:'ms',confidence:'verified',checked_at:100,
      routing:{complete:true,remaining:'500.123456789123456789',unit:'魔粒'}})];
    globalThis.ms = {id:'ms',kind:'modelscope',name:'ModelScope',enabled:true,has_api_key:true,cost_tier:'free'};
  `, context);
  const card = vm.runInContext('providerCard(ms)', context);
  assert.match(card, /余额 500\.123456789123456789 魔粒/);
  assert.doesNotMatch(card, /剩余额度未知/);
  assert.match(card, /读取额度|更新/);
  assert.equal(vm.runInContext('isPrivate(ms)', context), false);
  assert.equal(vm.runInContext('dailyTaskLabel(ms)', context), '登录领取魔粒');
  assert.match(vm.runInContext('modelScopeGuidance()', context), /24 小时/);
});

/** 启用魔搭必须提供令牌；编辑已配置服务时保留密钥，停用服务允许稍后补填。 */
test('modelscope form requires a token only for enabled unconfigured services', async () => {
  const { context, nodes } = modelFixture();
  context.URL = URL;
  const fields = { 'f-kind': 'openai_compat', 'f-url': 'https://api-inference.modelscope.cn/v1',
    'f-id': '', 'f-key': '', 'f-enabled': 'true', 'f-options': '', 'f-capabilities': '' };
  for (const [id, value] of Object.entries(fields)) nodes.set(id, { value });
  vm.runInContext('updateProviderForm()', context);
  assert.equal(nodes.get('modelscope-guidance').hidden, false);
  assert.equal(nodes.get('f-key').required, true);
  await assert.rejects(vm.runInContext('saveProvider()', context), /Access Token/);
  nodes.get('f-enabled').value = 'false';
  vm.runInContext('updateProviderForm()', context);
  assert.equal(nodes.get('f-key').required, false);
  nodes.get('f-enabled').value = 'true';
  nodes.get('f-id').value = 'ms';
  vm.runInContext("state.backends.push({id:'ms',has_api_key:true}); updateProviderForm()", context);
  assert.equal(nodes.get('f-key').required, false);
  assert.match(nodes.get('f-key').placeholder, /留空保留/);
  nodes.get('f-url').value = 'https://api.other.test/v1';
  vm.runInContext('updateProviderForm()', context);
  assert.equal(nodes.get('modelscope-guidance').hidden, true);
});

/** 详情页启用缺少令牌的服务时进入编辑表单，不直接把未授权服务交给路由。 */
test('modelscope activation opens token setup before enabling the service', async () => {
  const { context, nodes } = modelFixture();
  context.URL = URL;
  nodes.set('f-kind', { value: 'openai_compat' });
  nodes.set('f-url', { value: 'https://api-inference.modelscope.cn/v1' });
  nodes.set('f-id', { value: 'ms' });
  vm.runInContext(`
    state.backends = [{id:'ms',kind:'openai_compat',base_url:'https://api-inference.modelscope.cn/v1',has_api_key:false}];
    editProvider = (id) => { globalThis.editing = id; };
    toast = () => {};
    api = () => { throw new Error('unconfigured service must not be enabled'); };
  `, context);
  await vm.runInContext("patchProvider('ms',{enabled:true})", context);
  assert.equal(context.editing, 'ms');
  assert.equal(nodes.get('f-enabled').value, 'true');
  assert.equal(nodes.get('f-key').required, true);
});

/** 检查私有客户端名称翻译、明确免费以及无倍率的回退展示。 */
test('private catalogs keep names, free rates and unknown rates distinct', () => {
  const { context } = fixture();
  vm.runInContext(`
    globalThis.rates = ['qoder','workbuddy','autoglm','minimax_code'].map((kind) => {
      const backend = {kind, model_catalog:[
        {id:'route-id',display_name:'Auto',price_factor:0,original_price_factor:0.5},
        {id:'glm',display_name:'GLM-5'}
      ]};
      return [modelName(backend,'route-id'),modelRate(backend,'route-id'),
        modelOriginalRate(backend,'route-id'),modelRate(backend,'glm'),modelTitle(backend,'glm')];
    });
  `, context);
  assert.deepEqual(JSON.parse(JSON.stringify(context.rates)), Array(4).fill([
    '自动', '0×', '0.5×', '倍率未知', 'GLM-5 · 倍率未知',
  ]));
});

/** 精确快照、百分之一积分换算及多账号汇总均保留十八位小数。 */
test('quota display preserves exact snapshots, native units and tiny balances', () => {
  const { context } = fixture();
  vm.runInContext(`
    state.accounts = [{id:'a',backend_id:'glm',enabled:true},{id:'b',backend_id:'glm',enabled:true}];
    state.quota = [
      {backend_id:'glm',account_id:'a',confidence:'verified',checked_at:10,
        routing:{complete:true,remaining:'2118.800000120000000001',unit:'积分'}},
      {backend_id:'glm',account_id:'b',confidence:'verified',checked_at:11,
        routing:{complete:true,remaining:'0.000000000000000001',unit:'积分'}}
    ].map(normalizeQuota);
    globalThis.amount = quotaOf({id:'glm'}).remaining;
    globalThis.mini = normalizeQuota({remaining:1234,unit:'0.01 积分',
      routing:{complete:true,remaining:'1234',unit:'0.01 积分'}});
  `, context);
  assert.equal(context.amount, '2118.800000120000000002');
  assert.equal(vm.runInContext('quotaNumber(amount)', context), '2,118.800000120000000002');
  assert.equal(vm.runInContext('quotaNumber(state.quota[1].remaining)', context), '0.000000000000000001');
  assert.equal(context.mini.remaining, '12.34');
  assert.equal(context.mini.unit, '积分');
  assert.equal(context.mini.routing.unit, '0.01 积分');
  assert.equal(vm.runInContext("quotaUnits('1.0000000000000000001')", context), null);
  vm.runInContext("state.quota[1].confidence = 'unknown'", context);
  assert.equal(vm.runInContext("quotaOf({id:'glm'})", context), null);
});

/** 较旧的缓存轮询不能覆盖手动更新刚返回的结果与下次时间。 */
test('quota cache generations keep manual refresh results monotonic', () => {
  const { context } = fixture();
  vm.runInContext(`
    acceptQuota({quota:[{remaining:12,unit:'积分'}],auto_refresh:{generation:2,next_refresh_at:1000}});
    acceptQuota({quota:[{remaining:1,unit:'积分'}],auto_refresh:{generation:1,next_refresh_at:500}});
  `, context);
  assert.equal(vm.runInContext('state.quota[0].remaining', context), 12);
  assert.equal(vm.runInContext('state.quotaStatus.next_refresh_at', context), 1000);
});

/** 页面每分钟仅读本地缓存，隐藏后停轮询，编辑中的表单不被重绘。 */
test('quota polling reads only cached data and preserves active edits', async () => {
  const { context, timers } = fixture();
  context.document = {
    visibilityState: 'visible',
    getElementById: () => ({ open: true, querySelector: () => ({ dataset: { draft: 'editing' } }) }),
  };
  vm.runInContext(`
    state.drawerProvider = 'q';
    globalThis.paths = [];
    api = async (path) => { paths.push(path); return {quota:[],auto_refresh:{generation:1}}; };
    renderProviders = () => {};
    openProvider = () => { throw new Error('editing form must remain untouched'); };
    scheduleQuotaPoll();
  `, context);
  assert.equal(timers.size, 1);
  const [id, timer] = [...timers.entries()][0];
  assert.equal(timer.delay, 60000);
  timers.delete(id);
  await timer.callback();
  assert.deepEqual(JSON.parse(JSON.stringify(context.paths)), ['/api/quota/cached']);
  assert.equal(timers.size, 1);
  context.document.visibilityState = 'hidden';
  vm.runInContext('scheduleQuotaPoll()', context);
  assert.equal(timers.size, 0);
});

/** 页面停留或后台延迟后，到期报价不继续显示零倍率或旧优惠划线。 */
test('expired prices remove current and original discount labels', () => {
  const { context, at } = fixture();
  vm.runInContext(`globalThis.backend = {kind:'workbuddy',model_catalog:[
    {id:'glm',display_name:'GLM-5',price_factor:0,original_price_factor:0.69,price_valid_until:110}
  ]};`, context);
  assert.equal(vm.runInContext("modelRate(backend,'glm')", context), '0×');
  at(110000);
  assert.equal(vm.runInContext("modelRate(backend,'glm')", context), '倍率待刷新');
  assert.equal(vm.runInContext("modelOriginalRate(backend,'glm')", context), '');
  assert.ok(!vm.runInContext("modelRateBadge(backend,'glm')", context).includes('<s '));
});

/** 只保留最早边界的一个定时器；延迟触发后更新展示而不重新请求厂商。 */
test('one expiry timer refreshes all visible model surfaces after a late callback', () => {
  const { context, timers, at } = fixture();
  vm.runInContext(`
    state.backends = [{model_catalog:[{price_valid_until:120},{price_valid_until:110}]}];
    state.drawerProvider = 'workbuddy';
    globalThis.updates = [];
    fillModelSelectors = () => updates.push('selectors');
    renderModels = () => updates.push('models');
    renderProviders = () => updates.push('providers');
    openProvider = () => updates.push('details');
    globalThis.document = {getElementById:() => ({open:true})};
    globalThis.window = {SelectControls:{refresh:() => updates.push('dropdown')}};
    schedulePriceExpiry(); schedulePriceExpiry();
  `, context);
  assert.equal(timers.size, 1);
  const [id, timer] = [...timers.entries()][0];
  assert.equal(timer.delay, 10025);
  at(130000);
  timers.delete(id);
  timer.callback();
  assert.deepEqual(JSON.parse(JSON.stringify(context.updates)), [
    'selectors', 'models', 'providers', 'dropdown', 'details',
  ]);
  assert.equal(timers.size, 0);
});

/** MiniMax 登录域名必须与服务区域一致。 */
test('minimax browser login requires the official regional OAuth host', () => {
  const { context } = fixture();
  context.URL = URL;
  context.location = { origin: 'http://127.0.0.1:8317' };
  vm.runInContext(`
    globalThis.cn = {kind:'minimax_code',options:'{"region":"cn"}'};
    globalThis.flow = {automatic_account_save:true,
      session_id:'f176e9b3-6542-46b6-b7cb-d2f0e83140b1',
      login_url:'https://account.minimax.cn/oauth/authorize?code=test'};
  `, context);
  assert.equal(vm.runInContext('browserLoginUrl(flow,cn).origin', context), 'https://account.minimax.cn');
  for (const bad of [
    'https://example.com/oauth/authorize',
    'https://account.minimax.io/oauth/authorize',
    'https://account.minimax.cn.evil.test/oauth/authorize',
    'https://user:password@account.minimax.cn/oauth/authorize',
    'http://account.minimax.cn/oauth/authorize',
    'https://account.minimax.cn:1234/oauth/authorize',
  ]) {
    context.bad = bad;
    assert.throws(() => vm.runInContext('browserLoginUrl({...flow,login_url:bad},cn)', context));
  }
  assert.throws(() => vm.runInContext("browserLoginUrl(flow,{kind:'minimax_code',options:'{\"region\":\"global\"}'})", context));
});

/** 国内短信地址只能指向当前同源会话，不能串到其他窗口或外站。 */
test('autoglm browser login requires the current same-origin SMS session', () => {
  const { context } = fixture();
  context.URL = URL;
  context.location = { origin: 'http://127.0.0.1:8317' };
  vm.runInContext(`
    globalThis.cn = {kind:'autoglm',options:'{"region":"cn"}'};
    globalThis.flow = {kind:'sms_browser',automatic_account_save:true,
      session_id:'f176e9b3-6542-46b6-b7cb-d2f0e83140b1',
      login_url:'/auth/autoglm/f176e9b3-6542-46b6-b7cb-d2f0e83140b1'};
  `, context);
  assert.equal(vm.runInContext('browserLoginUrl(flow,cn).origin', context), 'http://127.0.0.1:8317');
  assert.equal(vm.runInContext("autoGlmDomestic({options:'{}'})", context), true);
  for (const bad of [
    'https://example.com/auth/autoglm/f176e9b3-6542-46b6-b7cb-d2f0e83140b1',
    '/auth/autoglm/another-session',
    'https://autoglm.zhipuai.cn/login',
    '/auth/autoglm/f176e9b3-6542-46b6-b7cb-d2f0e83140b1?redirect_uri=https://example.com',
    '/auth/autoglm/f176e9b3-6542-46b6-b7cb-d2f0e83140b1#proof',
    'http://127.0.0.1:9999/auth/autoglm/f176e9b3-6542-46b6-b7cb-d2f0e83140b1',
  ]) {
    context.bad = bad;
    assert.throws(() => vm.runInContext('browserLoginUrl({...flow,login_url:bad},cn)', context));
  }
  assert.throws(() => vm.runInContext("browserLoginUrl({...flow,kind:'oauth'},cn)", context));
  assert.throws(() => vm.runInContext("browserLoginUrl(flow,{kind:'autoglm',options:'{\"region\":\"global\"}'},'zai')", context));
});

/** 小浣熊授权只能回到本机一次性回调，拒绝替换域和重复参数。 */
test('raccoon browser login validates official host and loopback callback', () => {
  const { context } = fixture();
  context.URL = URL;
  context.location = { origin: 'http://127.0.0.1:8317' };
  context.raccoonUrl = `https://xiaohuanxiong.com/authorize?login_source=desktop&desktop_callback=loopback&state=${'b'.repeat(64)}&redirect=${encodeURIComponent('http://127.0.0.1:12345/auth/callback')}`;
  vm.runInContext(`globalThis.flow = {kind:'loopback_oauth',login_url:raccoonUrl};
    globalThis.backend = {kind:'raccoon'};`, context);
  assert.equal(vm.runInContext('browserLoginUrl(flow,backend).hostname', context), 'xiaohuanxiong.com');
  for (const url of [
    context.raccoonUrl.replace('xiaohuanxiong.com', 'xiaohuanxiong.com.evil.test'),
    context.raccoonUrl.replace('https:', 'http:'),
    context.raccoonUrl.replace('127.0.0.1', 'evil.test'),
    context.raccoonUrl.replace('12345', '999'),
    context.raccoonUrl.replace('12345', '65536'),
    context.raccoonUrl.replace('b'.repeat(64), 'short'),
    context.raccoonUrl + '&state=duplicate',
    context.raccoonUrl + '#secret',
  ]) {
    context.badUrl = url;
    assert.throws(() => vm.runInContext('browserLoginUrl({...flow,login_url:badUrl},backend)', context));
  }
});

/** TRAE 个人授权必须属于官网，并用随机登录标识绑定固定本机回调。 */
test('trae personal browser flow rejects foreign hosts and callbacks', () => {
  const { context } = fixture();
  context.URL = URL;
  context.location = { origin: 'http://127.0.0.1:8317' };
  context.traeUrl = `https://www.trae.cn/authorization?client_id=ono9krqynydwx5&auth_type=local&hide_saas_login=true&login_trace_id=${'a'.repeat(64)}&auth_callback_url=${encodeURIComponent('http://127.0.0.1:12345/authorize')}`;
  vm.runInContext("globalThis.flow={kind:'loopback_oauth',login_url:traeUrl};globalThis.backend={kind:'trae_personal'}", context);
  assert.equal(vm.runInContext('browserLoginUrl(flow,backend).hostname', context), 'www.trae.cn');
  for (const url of [
    context.traeUrl.replace('www.trae.cn', 'login.example.test'),
    context.traeUrl.replace('www.trae.cn', 'www.trae.cn.evil.test'),
    context.traeUrl.replace('127.0.0.1', 'evil.test'),
    context.traeUrl.replace('12345', '999'),
    context.traeUrl.replace('a'.repeat(64), 'short'),
    context.traeUrl + '&client_id=duplicate',
  ]) {
    context.badUrl = url;
    assert.throws(() => vm.runInContext('browserLoginUrl({...flow,login_url:badUrl},backend)', context));
  }
});

for (const kind of ['autoglm', 'minimax_code', 'monkeycode', 'raccoon', 'trae_personal']) {
  /** 登录弹窗跟踪持久操作，轮询完成后刷新账号且不重放授权。 */
test(`${kind} login popup polls for the automatically saved account`, async () => {
  const { context, timers } = fixture();
  context.URL = URL;
  context.AbortController = AbortController;
  context.location = { origin: 'http://127.0.0.1:8317' };
  const nodes = new Map();
  context.document = { getElementById: (id) => {
    if (!nodes.has(id)) nodes.set(id, { innerHTML: '', dataset: {}, open: true });
    return nodes.get(id);
  } };
  const popup = { opener: null, location: { href: '' }, close: () => {} };
  const opened = [];
  context.window = { open: (url) => { opened.push(url); popup.location.href = url; return popup; } };
  context.loginKind = kind;
  const loginUrl = kind === 'autoglm'
    ? '/auth/autoglm/f176e9b3-6542-46b6-b7cb-d2f0e83140b1'
    : kind === 'trae_personal'
    ? `https://www.trae.cn/authorization?client_id=ono9krqynydwx5&auth_type=local&hide_saas_login=true&login_trace_id=${'a'.repeat(64)}&auth_callback_url=${encodeURIComponent('http://127.0.0.1:12345/authorize')}`
    : kind === 'raccoon'
    ? `https://xiaohuanxiong.com/authorize?login_source=desktop&desktop_callback=loopback&state=${'a'.repeat(64)}&redirect=${encodeURIComponent('http://127.0.0.1:12345/auth/callback')}`
    : 'https://account.minimax.cn/oauth/authorize?code=test';
  context.loginUrl = loginUrl;
  vm.runInContext(`
    state.backends = [{id:'provider',kind:loginKind,name:loginKind,options:'{"region":"cn"}'}];
    globalThis.paths = [];
    modal = () => {};
    api = async (path) => {
      paths.push(path);
      return {operation_id:'op-login',action:'auth.start',service_id:'provider',status:'waiting_user',
        deadline:1000,next_action:{url:loginKind === 'monkeycode' ? null : loginUrl}};
    };
  `, context);
  await vm.runInContext(kind === 'monkeycode' ? "login('provider')" : "startBrowserLogin('provider')", context);
  if (kind !== 'monkeycode') assert.match(nodes.get('modal-body').innerHTML, /打开授权页/);
  assert.deepEqual(opened, []);
  assert.deepEqual(JSON.parse(JSON.stringify(context.paths)), ['/api/v2/operations']);
  assert.equal(timers.size, 1);
  assert.equal(vm.runInContext('state.login.operationId', context), 'op-login');
  assert.ok(nodes.get('modal-body').innerHTML.includes('等待授权'));
  assert.ok(!nodes.get('modal-body').innerHTML.includes('<input'));
  vm.runInContext(`
    globalThis.updates = [];
    api = async () => ({operation_id:'op-login',action:'auth.start',service_id:'provider',status:'succeeded',result:{name:'我的账号',models_synced:true}});
    refresh = async () => updates.push('refresh');
    openProvider = () => updates.push('provider');
    syncModels = async () => updates.push('sync');
    toast = () => {};
  `, context);
  const [id, timer] = [...timers.entries()][0];
  timers.delete(id);
  await timer.callback();
  assert.equal(vm.runInContext('state.login', context), null);
  assert.deepEqual(JSON.parse(JSON.stringify(context.updates)), ['refresh']);
  assert.ok(nodes.get('modal-body').innerHTML.includes('我的账号'));
  assert.equal(timers.size, 0);
  if (kind === 'monkeycode') {
    vm.runInContext(`
      api = async () => ({operation_id:'op-login-2',action:'auth.start',service_id:'provider',status:'waiting_user'});
    `, context);
    await vm.runInContext("startBrowserLogin('provider')", context);
    vm.runInContext(`
      api = async () => ({operation_id:'op-login-2',action:'auth.start',service_id:'provider',status:'succeeded',result:{name:'我的账号',models_synced:false,sync_error:'暂时网络故障'}});
      syncModels = async () => { throw new Error('saved sync result must not be repeated'); };
    `, context);
    const [nextId, nextTimer] = [...timers.entries()][0];
    timers.delete(nextId);
    await nextTimer.callback();
    assert.match(nodes.get('modal-body').innerHTML, /账号已保存/);
    assert.match(nodes.get('modal-body').innerHTML, /同步模型/);
    assert.match(nodes.get('modal-body').innerHTML, /暂时网络故障/);
  }
});
}

/** 关闭等待框会取消后台窗口，迟到的开始响应也会释放服务端会话。 */
test('monkeycode cancellation releases both active and late browser sessions', async () => {
  for (const late of [false, true]) {
    const { context, timers } = fixture();
    context.AbortController = AbortController;
    const nodes = new Map();
    context.document = { getElementById: (id) => {
      if (!nodes.has(id)) nodes.set(id, {innerHTML:'',dataset:{},open:true});
      return nodes.get(id);
    }};
    context.window = { open: () => { throw new Error('official login is opened by the proxy'); } };
    context.late = late;
    vm.runInContext(`
      state.backends = [{id:'mc',kind:'monkeycode',name:'MonkeyCode'}];
      modal = () => {};
      globalThis.requests = [];
      globalThis.release = null;
      api = async (path, options) => {
        requests.push({path,body:options?.body});
        if (path === '/api/v2/operations') {
          if (late) await new Promise((resolve) => { release = resolve; });
          return {operation_id:'op-cancel',action:'auth.start',status:'waiting_user',deadline:1000};
        }
        return {operation_id:'op-cancel',status:path.endsWith('/cancel') ? 'cancelled' : 'waiting_user'};
      };
    `, context);
    const started = vm.runInContext("startBrowserLogin('mc')", context);
    if (!late) await started;
    vm.runInContext('cancelLogin()', context);
    if (late) { context.release(); await started; }
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(vm.runInContext('state.login', context), null);
    assert.equal(timers.size, 0);
    assert.equal(context.requests.at(-1).path, '/api/v2/operations/op-cancel/cancel');
    assert.equal(context.requests.filter((request) => request.path === '/api/v2/operations').length, 1);
  }
});

/** 旧 WorkBuddy 目录的窗口字段只能展示为输入上限，新字段不使用旧值补齐。 */
test('token limits distinguish old input-only catalogs and new unknown fields', () => {
  const { context } = fixture();
  vm.runInContext(`
    globalThis.old = {kind:'workbuddy',model_catalog:[
      {id:'glm',capabilities:{context_window:1000000}}
    ]};
    globalThis.fresh = {kind:'workbuddy',model_catalog:[
      {id:'glm',capabilities:{context_window:1000000},
        token_limits:{max_input_tokens:1000000,max_output_tokens:8192}}
    ]};
  `, context);
  assert.equal(vm.runInContext("modelLimits(old,'glm').context_window", context), null);
  assert.equal(vm.runInContext("modelLimits(old,'glm').max_input_tokens", context), 1000000);
  assert.equal(vm.runInContext("modelLimits(old,'glm').legacy", context), true);
  assert.equal(vm.runInContext("modelLimits(fresh,'glm').context_window", context), null);
  assert.equal(vm.runInContext("modelLimits(fresh,'glm').legacy", context), false);
  assert.equal(vm.runInContext("modelLimitSummary({kind:'minimax_code'},'glm')", context), 'Token 限制未知');
});

/** 配置参考保留原始 Token 数和调用 ID，并携带适配器限制而不写成聊天参数。 */
test('model reference preserves tiers and explains adapter configuration limits', () => {
  const { context } = fixture();
  vm.runInContext(`
    state.backends = [{id:'qoder',kind:'qoder',model_catalog:[{id:'route/model',
      token_limits:{context_window:400000,max_input_tokens:180000,
        context_tiers:[{tokens:200000,is_default:true},{tokens:400000,is_default:false}]}
    }]}];
    globalThis.reference = modelConfigReference('qoder/route/model');
  `, context);
  const reference = JSON.parse(JSON.stringify(context.reference));
  assert.equal(reference.model, 'qoder/route/model');
  assert.equal(reference.context_window, 400000);
  assert.equal(reference.max_output_tokens, null);
  assert.deepEqual(reference.context_tiers, [
    { tokens: 200000, is_default: true }, { tokens: 400000, is_default: false },
  ]);
  assert.ok(reference.notes.some((note) => note.includes('180000') && note.includes('不会自动切换')));
  assert.ok(!Object.hasOwn(reference, 'max_tokens'));
  assert.equal(vm.runInContext("modelConfigReference('auto')", context), null);
});

/** 页面刷新不得覆盖未保存草稿，固定时间和窗口边界都有明确提示。 */
test('schedule refresh preserves unsaved edits and explains the time window', () => {
  const { context } = fixture();
  const nodes = new Map();
  context.document = { getElementById: (id) => {
    if (!nodes.has(id)) nodes.set(id, { value: '', textContent: '', checked: false });
    return nodes.get(id);
  } };
  vm.runInContext(`
    acceptSchedule({settings:{enabled:false,time:'10:00',random_minutes:60,revision:1},next_runs:[]});
    renderScheduleForm();
  `, context);
  assert.equal(nodes.get('schedule-time').value, '10:00');
  assert.ok(nodes.get('schedule-window').textContent.includes('10:00–11:00'));
  nodes.get('schedule-time').value = '23:30';
  nodes.get('schedule-random').value = '0';
  vm.runInContext(`
    state.scheduleDirty = true;
    acceptSchedule({settings:{enabled:true,time:'09:00',random_minutes:90,revision:2},next_runs:[]});
    renderScheduleForm();
  `, context);
  assert.equal(nodes.get('schedule-time').value, '23:30');
  assert.equal(nodes.get('schedule-random').max, 30);
  assert.ok(nodes.get('schedule-window').textContent.includes('固定执行'));
  assert.equal(nodes.get('schedule-save-status').textContent, '有未保存的修改');
  nodes.get('schedule-random').value = '31';
  vm.runInContext('renderScheduleWindow()', context);
  assert.ok(nodes.get('schedule-window').textContent.includes('不能超过 24:00'));
  vm.runInContext("acceptSchedule({settings:{revision:1}})", context);
  assert.equal(vm.runInContext('state.schedule.settings.revision', context), 2);
});

/** 保存调用真实 PUT 契约，请求期间的新输入继续保留为未保存草稿。 */
test('schedule save keeps edits made during the request', async () => {
  const { context } = fixture();
  const nodes = new Map([
    ['schedule-enabled', { checked: true }],
    ['schedule-time', { value: '10:00' }],
    ['schedule-random', { value: '60' }],
    ['schedule-form', { reportValidity: () => true }],
    ['schedule-window', {}], ['schedule-save-status', {}], ['schedule-save', {}],
  ]);
  context.document = { getElementById: (id) => nodes.get(id) };
  vm.runInContext(`
    globalThis.sent = [];
    busy = async (_, work) => work();
    toast = () => {};
    renderTasks = renderScheduleForm;
    api = async (path, options) => {
      sent.push([path, options]);
      document.getElementById('schedule-time').value = '12:00';
      return {settings:{...options.body,revision:2},next_runs:[]};
    };
  `, context);
  await vm.runInContext('saveSchedule({preventDefault(){},submitter:null})', context);
  const sent = JSON.parse(JSON.stringify(context.sent));
  assert.equal(sent[0][0], '/api/tasks/checkin-schedule');
  assert.equal(sent[0][1].method, 'PUT');
  assert.deepEqual(sent[0][1].body, { enabled: true, time: '10:00', random_minutes: 60 });
  assert.equal(nodes.get('schedule-time').value, '12:00');
  assert.equal(vm.runInContext('state.scheduleDirty', context), true);
});

/** 一键签到只提交一次启动请求，后续轮询只读进度，完成后恢复按钮并刷新记录。 */
test('batch checkin starts once and polls without replaying the action', async () => {
  const { context, timers } = fixture();
  const nodes = new Map();
  context.document = { getElementById: (id) => {
    if (!nodes.has(id)) nodes.set(id, { setAttribute() {}, removeAttribute() {} });
    return nodes.get(id);
  } };
  vm.runInContext(`
    state.accounts = [{id:'a',backend_id:'q',enabled:true}];
    state.runtime = {checkin_backends:['q']};
    globalThis.paths = [];
    globalThis.refreshed = 0;
    globalThis.run = {id:'batch',started_at:100,version:1,status:'running',
      accounts:[{backend_name:'Q',account_name:'A',status:'waiting',message:'等待',
        delay_ms:12345,amount:null}]};
    api = async (path, options = {}) => {
      paths.push([path,options.method || 'GET']);
      return {run:options.method === 'POST' ? run : {...run,version:2,status:'completed',
        accounts:[{...run.accounts[0],status:'succeeded',message:'已领取',amount:1}]}};
    };
    busy = async (_, work) => work();
    toast = () => {};
    renderTasks = renderBatch;
    refresh = async () => {refreshed++;};
  `, context);
  await vm.runInContext("checkinAll(document.getElementById('checkin-all'))", context);
  await vm.runInContext("checkinAll(document.getElementById('checkin-all'))", context);
  assert.equal(nodes.get('checkin-all').disabled, true);
  assert.equal(nodes.get('batch-progress').textContent, '进行中 · 0/1');
  assert.ok(nodes.get('batch-results').innerHTML.includes('随机间隔 12.3 秒'));
  assert.equal(timers.size, 1);
  const [id, timer] = [...timers.entries()][0];
  assert.equal(timer.delay, 2000);
  timers.delete(id);
  await timer.callback();
  assert.deepEqual(JSON.parse(JSON.stringify(context.paths)), [
    ['/api/tasks/checkin-all', 'POST'], ['/api/tasks/checkin-all', 'GET'],
  ]);
  assert.equal(nodes.get('checkin-all').disabled, false);
  assert.equal(nodes.get('batch-progress').textContent, '已完成 · 1/1');
  assert.equal(context.refreshed, 1);
  assert.equal(timers.size, 0);
});

/** 响应倒序不能回退已完成结果，也不能覆盖更新的批次。 */
test('batch response versions keep progress monotonic', () => {
  const { context } = fixture();
  vm.runInContext(`
    acceptBatch({run:{id:'one',started_at:100,version:5,status:'completed'}});
    acceptBatch({run:{id:'one',started_at:100,version:4,status:'running'}});
  `, context);
  assert.equal(vm.runInContext('state.batch.run.status', context), 'completed');
  vm.runInContext(`
    acceptBatch({run:{id:'two',started_at:200,version:1,status:'running'}});
    acceptBatch({run:{id:'one',started_at:100,version:6,status:'completed'}});
    acceptBatch({run:null});
  `, context);
  assert.equal(vm.runInContext('state.batch.run.id', context), 'two');
  vm.runInContext('acceptBatch({run:null},true)', context);
  assert.equal(vm.runInContext('state.batch.run', context), null);
});
