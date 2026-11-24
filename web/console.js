/* 控制台交互。使用同源管理 API；凭证不写入浏览器持久存储。 */
'use strict';
const $ = (id) => document.getElementById(id);
const state = {
  backends: [], accounts: [], records: [], tasks: [], usage: [], health: [], aliases: [],
  quota: null, quotaStatus: null, quotaTimer: null, presets: [], runtime: null, adminKey: '',
  updatedAt: null, drawerProvider: null, login: null, chat: null, chatImages: [], chatImageId: 0, importBundle: null, pricingTimer: null,
  recordStorage: null, recordGroup: null,
  schedule: null, scheduleDirty: false, scheduleSaving: false,
  batch: null, batchStarting: false, batchTimer: null, batchError: '',
  modelGroups: null, refreshPromise: null, refreshAgain: false,
  discardAction: null,
  gateway: null, gatewayDirty: false, gatewaySaving: false,
  plugins: [], configuration: null, operations: [], plan: null,
};
const pages = { overview: '概览', providers: '服务接入', models: '模型与调试', requests: '请求记录', tasks: '任务', settings: '设置与接入' };
const costs = { free: '免费', subscription: '套餐额度', paid: '按量计费', unknown: '费用未知' };
const icons = {
  grid: '<rect x="3" y="3" width="7" height="7" rx="1.4"/><rect x="14" y="3" width="7" height="7" rx="1.4"/><rect x="3" y="14" width="7" height="7" rx="1.4"/><rect x="14" y="14" width="7" height="7" rx="1.4"/>',
  layers: '<path d="m12 3 9 5-9 5-9-5 9-5Zm-9 9 9 5 9-5M3 16l9 5 9-5"/>',
  box: '<path d="m12 3 8 4.5v9L12 21l-8-4.5v-9L12 3Zm-8 4.5 8 5 8-5M12 12v9"/>',
  spark: '<path d="m12 3 2.8 6.2L21 12l-6.2 2.8L12 21l-2.8-6.2L3 12l6.2-2.8L12 3Z"/>',
  activity: '<path d="M3 12h4l3-7 4 14 3-7h4"/>',
  clock: '<circle cx="12" cy="12" r="8.5"/><path d="M12 7v5l3 2"/>',
  settings: '<path d="m9 3-.6 3-2 1.2L3.5 7 2 10l2.4 2-0.1 2L2 16l1.5 3 3-.3L9 20l.5 2h5l.5-2 2.5-1.3 3 .3 1.5-3-2.3-2-.1-2L22 10l-1.5-3-2.9.2-2-1.2L15 3H9Z" transform="translate(1 0) scale(.9)"/><circle cx="12" cy="12" r="3"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  refresh: '<path d="M20 7v5h-5M4 17v-5h5"/><path d="M5.6 7a7.5 7.5 0 0 1 12.5-2L20 8M4 16l1.9 3a7.5 7.5 0 0 0 12.5-2"/>',
  arrow: '<path d="M5 12h14m-5-5 5 5-5 5"/>',
  copy: '<rect x="8" y="8" width="12" height="13" rx="2"/><path d="M16 8V3H3v13h5"/>',
  search: '<circle cx="10.5" cy="10.5" r="6.5"/><path d="m16 16 5 5"/>',
  close: '<path d="m6 6 12 12M18 6 6 18"/>',
  route: '<circle cx="6" cy="5" r="2"/><circle cx="18" cy="5" r="2"/><circle cx="12" cy="19" r="2"/><path d="M6 7v4c0 2 6 1 6 6m6-10v4c0 2-6 1-6 6"/>',
  download: '<path d="M12 3v12m-4-4 4 4 4-4M4 16v5h16v-5"/>',
  upload: '<path d="M12 16V4m-4 4 4-4 4 4M4 16v5h16v-5"/>',
  key: '<circle cx="8" cy="9" r="5"/><path d="m12 13 8 8m-4-4 3-3m-6 0 3-3"/>',
  more: '<circle cx="5" cy="12" r=".8"/><circle cx="12" cy="12" r=".8"/><circle cx="19" cy="12" r=".8"/>',
};

/** 转义所有插入 HTML 的业务字段。 */
function esc(value) {
  return String(value ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}
/** 返回内联图标，不加载第三方资源。 */
function icon(name) {
  return `<svg class="icon" viewBox="0 0 24 24" aria-hidden="true">${icons[name] || icons.box}</svg>`;
}
/** 初始化静态标记中的图标。 */
function hydrateIcons() {
  document.querySelectorAll('[data-icon]').forEach((node) => { node.outerHTML = icon(node.dataset.icon); });
}
/** 对数字统一排版，缺失值保持未知。 */
function number(value) {
  return value == null ? '—' : new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 2 }).format(value);
}
/** 格式化服务端秒时间。 */
function time(value) {
  return value ? new Date(value * 1000).toLocaleTimeString('zh-CN', { hour12: false }) : '—';
}
/** 取服务的显示名称。 */
function backendName(id) {
  return state.backends.find((b) => b.id === id)?.name || id || '未解析';
}
/** 返回模型地址的人类可读名称，不改动真实 ID。 */
function modelLabel(address) {
  const model = modelAt(address);
  return model ? `${model.backend.name} / ${modelName(model.backend, model.id)}` : address || '未解析模型';
}
/** 展示同步的官方模型名，私有客户端的 Auto 档位统一显示中文。 */
function modelName(backend, id) {
  const name = backend.model_catalog?.find((model) => model.id === id)?.display_name;
  const display = typeof name === 'string' && name.trim() ? name.trim() : id;
  if (isPrivate(backend) && display.toLowerCase() === 'auto') return '自动';
  const modes = { Auto: '自动', Ultimate: '极致', Performance: '快速', Efficient: '经济' };
  return backend.kind === 'qoder' && Object.hasOwn(modes, display) ? modes[display] : display;
}
/** 到达上游活动边界后，缓存报价需要重新同步才能确认。 */
function modelPriceExpired(model) {
  return typeof model?.price_valid_until === 'number' && model.price_valid_until * 1000 <= Date.now();
}
/** 展示上游确认的积分倍率，保留零值；缺失或非法值显示未知。 */
function modelRate(backend, id) {
  const model = backend.model_catalog?.find((model) => model.id === id);
  if (modelPriceExpired(model)) return '倍率待刷新';
  const factor = model?.price_factor;
  return typeof factor === 'number' && Number.isFinite(factor) && factor >= 0
    ? `${factor}×` : isPrivate(backend) ? '倍率未知' : '';
}
/** 仅在上游原倍率高于当前倍率时展示优惠前数值。 */
function modelOriginalRate(backend, id) {
  const model = backend.model_catalog?.find((model) => model.id === id);
  if (modelPriceExpired(model)) return '';
  const original = model?.original_price_factor;
  const current = model?.price_factor;
  return typeof original === 'number' && Number.isFinite(original)
    && typeof current === 'number' && Number.isFinite(current) && current >= 0 && original > current
    ? `${original}×` : '';
}
/** 构造模型列表中的倍率徽标，原倍率与当前倍率分别展示。 */
function modelRateBadge(backend, id) {
  const rate = modelRate(backend, id);
  if (!rate) return '';
  const original = modelOriginalRate(backend, id);
  return `<span class="model-rate" title="上游积分消耗倍率">${original ? `<s class="model-original-rate">${esc(original)}</s>` : ''}${esc(rate)}</span>`;
}
/** 为模型名称补充倍率，供原生下拉和紧凑标签使用。 */
function modelTitle(backend, id) {
  const rate = modelRate(backend, id);
  return modelName(backend, id) + (rate ? ` · ${rate}` : '');
}
/** 将目录能力与用户显式覆盖合并，未知能力保持未声明。 */
function modelCapabilities(backend, id) {
  const capabilities = { ...(backend.model_catalog?.find((model) => model.id === id)?.capabilities || {}) };
  try {
    const overrides = JSON.parse(backend.capabilities || '{}');
    for (const key of ['images', 'tools', 'parallel_tools', 'reasoning']) {
      if (typeof overrides[key] === 'boolean') capabilities[key] = overrides[key];
    }
  } catch { /* 配置校验由服务端执行，页面保留目录信息。 */ }
  return capabilities;
}
/** 显示已声明的输入和调用能力，用于选模和试聊。 */
function modelCapabilitySummary(backend, id) {
  const capabilities = modelCapabilities(backend, id);
  return [capabilities.images ? '图片输入' : '文本输入', capabilities.tools ? '工具调用' : '', capabilities.reasoning ? '推理' : ''].filter(Boolean).join(' · ');
}
/** 只接受模型目录中的正整数 Token 数，不把缺失值转换成零。 */
function validTokenCount(value) {
  return Number.isInteger(value) && value > 0 && value <= 4294967295 ? value : null;
}
/** 读取新目录限制；旧 WorkBuddy 缓存中的窗口字段实际是输入上限。 */
function modelLimits(backend, id) {
  const model = backend.model_catalog?.find((model) => model.id === id);
  const raw = model?.token_limits || {};
  const limits = {
    context_window: validTokenCount(raw.context_window),
    max_input_tokens: validTokenCount(raw.max_input_tokens),
    max_output_tokens: validTokenCount(raw.max_output_tokens),
    context_tiers: (Array.isArray(raw.context_tiers) ? raw.context_tiers : [])
      .filter((tier) => validTokenCount(tier?.tokens))
      .map((tier) => ({ tokens: tier.tokens, is_default: tier.is_default === true })),
    legacy: false,
  };
  const old = validTokenCount(model?.capabilities?.context_window);
  if (!Object.keys(raw).length && old) {
    limits[backend.kind === 'workbuddy' ? 'max_input_tokens' : 'context_window'] = old;
    limits.legacy = true;
  }
  return limits;
}
/** 以原始 Token 数显示限制，避免 K/M 缩写引入单位歧义。 */
function tokenLabel(value) {
  return value == null ? '未知' : `${number(value)} tokens`;
}
/** 模型行先显示已知窗口或输入限制，完整输入与输出在参数页查看。 */
function modelLimitSummary(backend, id) {
  const limits = modelLimits(backend, id);
  const known = [];
  if (limits.context_window) known.push(`窗口 ${number(limits.context_window)}`);
  if (limits.max_input_tokens) known.push(`输入 ≤ ${number(limits.max_input_tokens)}`);
  if (limits.max_output_tokens) known.push(`输出 ≤ ${number(limits.max_output_tokens)}`);
  return known.length ? known.join(' · ') + ' tokens' : 'Token 限制未知';
}
/** 将模型能力与 Token 限制组合为选模页的摘要。 */
function modelSummary(backend, id) {
  return modelCapabilitySummary(backend, id) + ' · ' + modelLimitSummary(backend, id);
}
/** 按实际适配器行为说明接入配置与上游档位之间的边界。 */
function modelLimitNotes(backend, id) {
  const limits = modelLimits(backend, id);
  const notes = ['窗口、输入上限和输出上限分别配置；未知表示上游目录未提供数值。'];
  if (limits.legacy) notes.push('当前数值来自旧目录缓存；同步目录可补齐字段和可选档位。');
  if (backend.kind === 'qoder') notes.push('Qoder 档位为上游目录声明。当前适配器固定声明 max_input_tokens=180000；客户端窗口配置不会自动切换上游档位，目录最大值不代表当前请求可用输入量。');
  if (backend.kind === 'autoglm') notes.push('AutoGLM 不透传 max_tokens；Auto 使用目录输出上限或 options.routing.provider_output_reserve 预留，两者未知时排除候选。');
  return notes;
}
/** 解析完整模型地址，保留模型 ID 中可能存在的斜杠。 */
function modelAt(address) {
  const alias = state.aliases.find((item) => item.alias === address);
  if (alias) {
    const backend = state.backends.find((item) => item.id === alias.backend_id);
    return backend ? { backend, id: alias.model_id } : null;
  }
  const backend = state.backends.find((backend) => address?.startsWith(backend.id + '/'));
  return backend ? { backend, id: address.slice(backend.id.length + 1) } : null;
}
/** 获取持久主调用名称，内部地址仅作为升级期间的兼容回退。 */
function modelCallName(backend, id) {
  return state.aliases.find((item) => item.backend_id === backend.id && item.model_id === id && item.is_primary)?.alias || `${backend.id}/${id}`;
}
/** 生成客户端模型配置参考；这些字段不是标准聊天请求参数。 */
function modelConfigReference(address) {
  const model = modelAt(address);
  if (!model) return null;
  const { legacy, ...limits } = modelLimits(model.backend, model.id);
  return {
    model: modelCallName(model.backend, model.id),
    capabilities: modelCapabilities(model.backend, model.id),
    ...limits,
    notes: modelLimitNotes(model.backend, model.id),
  };
}
/** 展示模型参数详情并提供配置参考复制入口，停用模型同样可查看。 */
function openModelDetails(address) {
  const model = modelAt(address);
  if (!model) return;
  const { backend: b, id } = model;
  const callName = modelCallName(b, id);
  let previousAlias = callName;
  const limits = modelLimits(b, id);
  const capabilities = modelCapabilities(b, id);
  state.drawerProvider = null;
  drawer(modelName(b, id), 'MODEL PARAMETERS', `
    <div class="drawer-section"><h3>调用名称</h3><div class="copy-field"><code>${esc(callName)}</code><button class="icon-button" data-action="copy-call-name" data-id="${esc(callName)}" aria-label="复制调用名称">${icon('copy')}</button></div><p class="hint">填入 SDK 的 model 字段；修改服务名或同步官方显示名后仍保持稳定。</p>
    <form id="alias-form"><label>自定义调用名称<input id="alias-name" value="${esc(callName)}" required maxlength="96" pattern="[a-z0-9][a-z0-9._-]*" autocomplete="off"></label><p class="hint">使用小写字母、数字、点、下划线或短横线。保存后旧名称继续可调用。</p><button id="alias-save" class="button small" type="submit">保存调用名称</button></form></div>
    <details class="drawer-section"><summary>原始路由与历史名称</summary><p class="hint">所属服务：${esc(b.name)}</p><pre>${esc(b.id + '/' + id)}</pre><p class="hint">兼容名称：${esc(state.aliases.filter((item) => item.backend_id === b.id && item.model_id === id && !item.is_primary).map((item) => item.alias).join('、') || '无' )}</p></details>
    <dl class="detail-summary">${[['images', '图片输入'], ['tools', '工具调用'], ['parallel_tools', '并行工具'], ['reasoning', '推理']].map(([key, label]) => `<div><dt>${label}</dt><dd>${capabilities[key] === true ? '支持' : '未声明支持'}</dd></div>`).join('')}<div><dt>上下文窗口${limits.context_tiers.length ? '（最大档位）' : ''}</dt><dd>${esc(tokenLabel(limits.context_window))}</dd></div><div><dt>最大输入</dt><dd>${esc(tokenLabel(limits.max_input_tokens))}</dd></div><div><dt>最大输出</dt><dd>${esc(tokenLabel(limits.max_output_tokens))}</dd></div><div><dt>默认窗口档位</dt><dd>${esc(tokenLabel(limits.context_tiers.find((tier) => tier.is_default)?.tokens))}</dd></div></dl>
    <div class="drawer-section"><h3>可选上下文档位</h3><p class="hint">${limits.context_tiers.length ? limits.context_tiers.map((tier) => `${tokenLabel(tier.tokens)}${tier.is_default ? '（默认）' : ''}`).join(' · ') : '上游目录未提供档位列表'}</p></div>
    ${modelLimitNotes(b, id).map((note) => `<p class="hint">${esc(note)}</p>`).join('')}
    <div class="drawer-section"><div class="section-heading"><h3>客户端模型配置参考</h3><button class="text-button" data-action="copy-model-params" data-id="${esc(address)}">${icon('copy')}复制参数</button></div><p class="hint">按客户端的配置字段映射使用；以下 JSON 是目录参考，不能直接作为聊天请求发送。</p><pre class="model-config-code">${esc(JSON.stringify(modelConfigReference(address), null, 2))}</pre></div>
    <div class="actions"><button class="button" data-action="provider" data-id="${esc(b.id)}">服务与目录同步</button>${b.enabled ? `<button class="button primary" data-action="select-model" data-id="${esc(callName)}">试聊此模型</button>` : ''}</div>`);
  trackDraft($('alias-form'));
  $('alias-form').addEventListener('submit', (event) => {
    event.preventDefault();
    busy($('alias-save'), async () => {
      const input = $('alias-name').value.trim();
      const result = await api('/api/model-aliases', { method: 'PUT', body: { backend_id: b.id, model_id: id, alias: input, previous_alias: previousAlias } });
      previousAlias = input;
      state.aliases = result.aliases;
      fillModelSelectors(); renderModels(); renderExample(); window.SelectControls?.refresh();
      // 保存期间继续输入的内容保留；下一次提交需读取新的主名称。
      if ($('alias-name')?.value.trim() === input) {
        $('alias-form').dataset.draft = formSignature($('alias-form'));
        openModelDetails(input);
      } else {
        toast('调用名称已保存；新输入仍为草稿，可继续保存');
        return;
      }
      toast('调用名称已保存，旧名称继续可用');
    });
  });
}
/** 在接入页和试聊页随所选模型更新限制摘要，不访问上游。 */
function renderSelectedLimits(selectId, targetId) {
  const target = $(targetId);
  if (!target) return;
  const address = $(selectId).value;
  const model = modelAt(address);
  target.innerHTML = model ? `<span>${esc(modelSummary(model.backend, model.id))}</span><button type="button" class="text-button" data-action="model-detail" data-id="${esc(address)}">查看参数与配置参考</button>` : '<span>自动路由的限制取决于实际选中的模型；含图请求只选择支持图片的模型。</span>';
}
/** 查找已安装插件的真实契约，不从预设推断能力。 */
function providerPlugin(backend) {
  return state.plugins.find((plugin) => plugin.id === (backend?.plugin_id || backend?.kind));
}
/** 生成幂等键和服务标识，兼容通过 HTTP 访问的非安全上下文。 */
function managementId() {
  if (crypto.randomUUID) return crypto.randomUUID();
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 15) | 64;
  bytes[8] = (bytes[8] & 63) | 128;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
/** 账号表单由插件声明的账号能力决定。 */
function isPrivate(backend) {
  return providerPlugin(backend)?.ui_group === 'reverse';
}
/** 服务分类仅用于展示，调度仍使用原有后端类型与费用层级。 */
function providerCategory(backend) {
  return (providerPlugin(backend)?.ui_group || backend?.ui_group) === 'reverse' ? '逆向 API' : '模型网关';
}
/** 将预设合并为一个分组入口，自定义网关也从同一入口选择。 */
function providerPresetOptions() {
  return ['模型网关', '逆向 API'].map((category) => {
    const options = state.presets.filter((preset) => providerCategory(preset) === category).map((preset) =>
      `<option value="${esc(preset.id)}" data-description="${esc(isPrivate(preset) ? '账号授权 · ' + (costs[preset.cost_tier] || '费用未知') : 'API Key · ' + (costs[preset.cost_tier] || '费用未知'))}">${esc(preset.name)}</option>`).join('');
    const withoutPreset = state.plugins.filter((plugin) => !state.presets.some((preset) => (preset.plugin_id || preset.kind) === plugin.id)
      && providerCategory({ plugin_id: plugin.id }) === category && plugin.id !== 'openai_compat')
      .map((plugin) => `<option value="plugin:${esc(plugin.id)}">${esc(plugin.display_name)}</option>`).join('');
    return `<optgroup label="${category}" data-group-id="${category}">${category === '模型网关' ? '<option value="" data-description="填写 OpenAI 兼容接口地址与密钥">自定义网关</option>' : ''}${options}${withoutPreset}</optgroup>`;
  }).join('');
}
/** 根据官方推理端点识别魔搭，编辑已有服务时也能显示接入说明。 */
function isModelScope(backend) {
  if (backend?.kind === 'modelscope') return true;
  if (backend?.kind !== 'openai_compat') return false;
  try {
    const url = new URL(backend.base_url);
    return url.origin === 'https://api-inference.modelscope.cn' && !url.username && !url.password
      && url.pathname.replace(/\/+$/, '') === '/v1';
  } catch { return false; }
}
/** 展示官方令牌入口及当前额度规则，不用本地 Token 用量推算魔粒余额。 */
function modelScopeGuidance() {
  return `<div class="notice"><strong>使用 ModelScope Access Token</strong><p>先在魔搭绑定已完成实名认证的阿里云账号，再把官方访问令牌填入 API Key。</p><div class="button-row"><a class="button small" href="https://modelscope.cn/my/myaccesstoken" target="_blank" rel="noopener noreferrer">获取访问令牌 ↗</a><a class="button small" href="https://www.modelscope.cn/docs/magicube/intro" target="_blank" rel="noopener noreferrer">查看魔粒规则 ↗</a></div><p>余额读取官方可用魔粒，冻结金额单独展示。任务页支持每日登录：当前每日活跃奖励 200 魔粒，绑定阿里云后另有 50 魔粒，以接口实际发放为准；这两项奖励自发放起 24 小时有效。</p><p>同步目录会从官方任务类型识别图片理解能力，并排除已知图像生成等非聊天模型。工具和 Token 限制按上游明确声明显示，未知限制可在高级配置中补充。</p></div>`;
}
/** API Key 服务也可支持官方账号余额查询。 */
function hasAccountQuota(backend) {
  return isPrivate(backend) || isModelScope(backend);
}
/** 单个定时器更新报价展示，不向上游发请求，也不清空试聊内容。 */
function schedulePriceExpiry() {
  clearTimeout(state.pricingTimer);
  const now = Date.now();
  const boundaries = state.backends.flatMap((backend) => backend.model_catalog || [])
    .map((model) => model.price_valid_until).filter((until) => typeof until === 'number' && until * 1000 > now);
  if (!boundaries.length) { state.pricingTimer = null; return; }
  const delay = Math.min(Math.min(...boundaries) * 1000 - now + 25, 2147483647);
  state.pricingTimer = setTimeout(() => {
    fillModelSelectors();
    renderModels();
    renderProviders();
    window.SelectControls?.refresh();
    if (state.drawerProvider && $('drawer').open) openProvider(state.drawerProvider, false);
    schedulePriceExpiry();
  }, delay);
}
/** 解析配置模型并去重。 */
function modelsOf(backend) {
  return [...new Set((backend.models || '').split(',').map((m) => m.trim()).filter(Boolean))];
}
/** 获取服务所属账号。 */
function accountsOf(id) {
  return state.accounts.filter((a) => a.backend_id === id);
}
/** 解析十八位十进制余额，汇总不经过浮点数。 */
function quotaUnits(value) {
  const match = /^(-?)(\d+)(?:\.(\d{1,18}))?$/.exec(String(value ?? ''));
  if (!match) return null;
  const units = BigInt(match[2]) * 10n ** 18n + BigInt((match[3] || '').padEnd(18, '0'));
  return match[1] ? -units : units;
}
/** 将定点余额还原为十进制，不舍弃微小余额。 */
function quotaString(units) {
  const sign = units < 0n ? '-' : '';
  const absolute = units < 0n ? -units : units;
  const fraction = String(absolute % 10n ** 18n).padStart(18, '0').replace(/0+$/, '');
  return sign + String(absolute / 10n ** 18n) + (fraction ? '.' + fraction : '');
}
/** 积分展示保留完整小数，仅对整数部分加千位分隔。 */
function quotaNumber(value) {
  const units = quotaUnits(value);
  if (units == null) return '—';
  const [whole, fraction] = quotaString(units).split('.');
  return whole.replace(/\B(?=(\d{3})+(?!\d))/g, ',') + (fraction ? '.' + fraction : '');
}
/** 优先展示精确快照，百分之一积分只在界面换算，保留原路由单位。 */
function normalizeQuota(row) {
  if (row.routing?.complete && row.routing.remaining != null && quotaUnits(row.routing.remaining) != null) {
    row.remaining = row.routing.remaining;
    row.unit = row.routing.unit;
  }
  if (row.unit === '0.01 积分') {
    for (const key of ['used', 'total', 'remaining']) {
      const units = quotaUnits(row[key]);
      if (units != null) row[key] = quotaString(units / 100n);
    }
    row.unit = '积分';
  }
  return row;
}
/** 读取已知额度，缺失账号或部分查询失败时不作完整汇总。 */
function quotaOf(backend) {
  const accounts = accountsOf(backend.id).filter((a) => a.enabled);
  const rows = (state.quota || []).filter((q) => q.backend_id === backend.id && accounts.some((a) => a.id === q.account_id));
  if (!accounts.length || rows.length !== accounts.length || rows.some((q) => q.confidence !== 'verified' || q.remaining == null && (q.total == null || q.used == null))) return null;
  if (new Set(rows.map((q) => q.unit)).size > 1) return null;
  if (rows.every((q) => q.remaining != null)) {
    const amounts = rows.map((q) => quotaUnits(q.remaining));
    if (amounts.some((amount) => amount == null)) return null;
    return { remaining: quotaString(amounts.reduce((sum, amount) => sum + amount, 0n)), unit: rows[0].unit || '', checked: Math.min(...rows.map((q) => q.checked_at)) };
  }
  return { total: rows.reduce((n, q) => n + q.total, 0), used: rows.reduce((n, q) => n + q.used, 0), unit: rows[0].unit || '', checked: Math.min(...rows.map((q) => q.checked_at)) };
}
/** 构造状态标签；状态文字必须说明实际证据范围。 */
function badge(text, kind = '') {
  return `<span class="badge ${kind}"><span class="badge-dot"></span>${esc(text)}</span>`;
}
/** 展示含后续动作的空状态。 */
function empty(title, message, action = '') {
  return `<div class="empty">${icon('box')}<h3>${esc(title)}</h3><p>${esc(message)}</p>${action}</div>`;
}
/** 提示不遮挡页面；对话框打开时也把错误呈现在当前上下文。 */
function toast(message, error = false) {
  const node = document.createElement('div');
  node.className = `toast${error ? ' error' : ''}`;
  const text = document.createElement('span');
  text.textContent = message;
  const close = document.createElement('button');
  close.className = 'icon-button';
  close.setAttribute('aria-label', '关闭提示');
  close.innerHTML = icon('close');
  close.addEventListener('click', () => node.remove());
  node.append(text, close);
  $('toasts').append(node);
  if (error) {
    const host = $('modal').open ? $('modal-body') : $('drawer').open ? $('drawer-body') : null;
    if (host) {
      let notice = host.querySelector('.action-error');
      if (!notice) { notice = document.createElement('div'); notice.className = 'notice error action-error'; notice.setAttribute('role', 'alert'); host.prepend(notice); }
      notice.textContent = message;
    }
  }
  setTimeout(() => node.remove(), error ? 10000 : 5000);
}
/** 调用同源 JSON API，统一处理超时、状态码和业务错误。 */
async function api(path, { method = 'GET', body, signal, timeout = 30000, inference = false } = {}) {
  const controller = new AbortController();
  const abort = () => controller.abort(signal.reason);
  if (signal) { if (signal.aborted) abort(); else signal.addEventListener('abort', abort, { once: true }); }
  const timer = setTimeout(() => controller.abort(new DOMException('请求超时', 'TimeoutError')), timeout);
  const headers = { 'Content-Type': 'application/json' };
  if (inference) {
    const key = $('play-key').value.trim();
    if (key) headers.Authorization = `Bearer ${key}`;
  } else if (state.adminKey) headers['X-Admin-Key'] = state.adminKey;
  try {
    const response = await fetch(path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body), signal: controller.signal });
    let data;
    try { data = await response.json(); } catch { throw new Error(`接口未返回 JSON（HTTP ${response.status}）`); }
    if (!response.ok || data.error && !data.operation_id) {
      const error = new Error(data.error?.message || data.message || `请求失败（HTTP ${response.status}）`);
      error.code = data.error?.code;
      if (error.code === 'invalid_admin_key') throw Object.assign(new Error('需要管理密钥，请前往「设置与接入」连接控制台。'), { code: error.code });
      throw error;
    }
    return data;
  } catch (error) {
    if (controller.signal.aborted) throw new Error(signal?.aborted ? '操作已取消' : '请求超时，请检查服务状态');
    throw error;
  } finally { clearTimeout(timer); signal?.removeEventListener('abort', abort); }
}
/** 为点击操作提供忙碌状态和统一失败提示。 */
async function busy(button, work) {
  if (button?.disabled) return;
  if (button) { button.disabled = true; button.setAttribute('aria-busy', 'true'); }
  try { return await work(); } catch (error) { toast(error.message, true); }
  finally { if (button) { button.disabled = false; button.removeAttribute('aria-busy'); } }
}
/** 读取本地快照，不因导航自动发起上游推理。 */
function refresh() {
  if (state.refreshPromise) { state.refreshAgain = true; return state.refreshPromise; }
  state.refreshPromise = (async () => {
    do {
      state.refreshAgain = false;
      await refreshSnapshot();
    } while (state.refreshAgain);
  })().finally(() => { state.refreshPromise = null; });
  return state.refreshPromise;
}
/** 合并并发刷新，避免旧响应覆盖后发的读取或编辑。 */
async function refreshSnapshot() {
  try {
    const [backends, accounts, usage, records, tasks, health, presets, runtime, schedule, batch, aliases, quota, gateway, plugins, configuration, operations] = await Promise.all([
      api('/api/backends'), api('/api/accounts'), api('/api/usage'), api('/api/records'),
      api('/api/tasks?limit=100'), api('/api/health/backends'), api('/api/presets'), api('/api/console/status'),
      api('/api/tasks/checkin-schedule'),
      api('/api/tasks/checkin-all'),
      api('/api/model-aliases'),
      api('/api/quota/cached'),
      api('/api/gateway'),
      api('/api/v2/plugins'), api('/api/v2/config'), api('/api/v2/operations'),
    ]);
    acceptSchedule(schedule);
    acceptBatch(batch);
    acceptQuota(quota);
    state.gateway = gateway;
    Object.assign(state, { plugins: plugins.plugins, configuration, operations: operations.operations });
    Object.assign(state, { backends: backends.backends, accounts: accounts.accounts.filter((account) => backends.backends.some((backend) => backend.id === account.backend_id)), aliases: aliases.aliases, usage: usage.usage, records: records.records, recordStorage: records.persistence, tasks: tasks.tasks, health: health.backends, presets: presets.presets, runtime, updatedAt: new Date() });
    schedulePriceExpiry();
    $('global-error').hidden = true;
    $('service-status').textContent = '本地服务已连接';
    $('service-dot').className = 'status-dot online';
    render();
    scheduleBatchPoll();
    scheduleQuotaPoll();
  } catch (error) {
    $('global-error').textContent = `${error.message} 已显示的数据可能不是最新状态。`;
    $('global-error').hidden = false;
    $('service-status').textContent = error.code === 'invalid_admin_key' ? '等待管理鉴权' : '连接未完成';
    $('service-dot').className = 'status-dot offline';
    throw error;
  }
}
/** 接受较新的持久缓存，慢速页面轮询不能覆盖手动更新后的结果。 */
function acceptQuota(result) {
  if (state.quotaStatus && (result.auto_refresh?.generation ?? 0) < state.quotaStatus.generation) return;
  state.quota = (result.quota || []).map(normalizeQuota);
  state.quotaStatus = result.auto_refresh || null;
}
/** 用日期和时间说明十二小时任务的最近结果及下次执行。 */
function renderQuotaStatus() {
  const node = $('quota-refresh-status');
  if (!node) return;
  const status = state.quotaStatus;
  const timestamp = (value) => value == null ? '尚未更新' : new Intl.DateTimeFormat('zh-CN', {
    month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false,
  }).format(value * 1000);
  node.textContent = !status ? '额度自动更新：每 12 小时 · 正在读取状态'
    : status.in_progress ? '额度自动更新：每 12 小时 · 正在更新账号额度'
    : `额度自动更新：每 12 小时 · 最近 ${timestamp(status.last_completed_at)} · 下次 ${timestamp(status.next_refresh_at)}${status.running ? '' : ' · 后台未运行'}${status.last_error ? ' · ' + status.last_error : ''}`;
}
/** 页面仅读取本地缓存，后台更新不打断编辑或试聊。 */
async function refreshQuotaCache() {
  acceptQuota(await api('/api/quota/cached'));
  renderProviders();
  if (state.drawerProvider && $('drawer')?.open && !$('drawer').querySelector('form[data-draft]')) openProvider(state.drawerProvider, false);
}
/** 可见页面每分钟同步本地快照，厂商查询由服务端十二小时任务负责。 */
function scheduleQuotaPoll() {
  clearTimeout(state.quotaTimer);
  state.quotaTimer = null;
  if (document.visibilityState === 'hidden') return;
  state.quotaTimer = setTimeout(async () => {
    state.quotaTimer = null;
    try { await refreshQuotaCache(); } catch { /* 保留有时间标记的旧快照，下次继续读取。 */ }
    finally { scheduleQuotaPoll(); }
  }, 60000);
}
/** 手动更新复用服务端批处理，并立即展示结果及下次更新时间。 */
async function refreshQuota() {
  acceptQuota(await api('/api/quota', { timeout: 120000 }));
  renderProviders();
  if (state.drawerProvider && $('drawer').open) openProvider(state.drawerProvider, false);
  const unknown = state.quota.filter((q) => q.confidence !== 'verified').length;
  toast(unknown ? `额度已更新，${unknown} 个账号暂未读取成功` : '账号额度已更新', !!unknown);
}
/** 切换页面，保留 hash 和浏览器返回导航。 */
function navigate() {
  window.SelectControls?.close();
  const page = location.hash.slice(1) in pages ? location.hash.slice(1) : 'overview';
  document.querySelectorAll('.page').forEach((node) => { node.hidden = node.id !== `page-${page}`; });
  document.querySelectorAll('[data-nav]').forEach((node) => {
    const active = node.dataset.nav === page;
    node.classList.toggle('active', active);
    if (active) node.setAttribute('aria-current', 'page'); else node.removeAttribute('aria-current');
  });
  $('breadcrumb-page').textContent = pages[page];
  document.title = `${pages[page]} · fakemodelproxy`;
  window.scrollTo({ top: 0, behavior: 'instant' });
}
/** 绘制已取到的快照，不清空正在输入的表单和聊天。 */
function render() {
  const configured = state.backends.reduce((n, b) => n + modelsOf(b).length, 0);
  const enabled = state.backends.filter((b) => b.enabled).length;
  const activeAccounts = state.accounts.filter((a) => a.enabled && a.has_credential).length;
  const tokens = state.usage.reduce((n, r) => n + r.used_tokens, 0);
  const metrics = [
    ['已启用服务', enabled, `/ ${state.backends.length}`, `${state.backends.length} 个服务已配置`, 'layers'],
    ['已配置模型', configured, '个', '按服务区分同名模型', 'spark'],
    ['可选账号', activeAccounts, '个', '已配置凭证且已启用', 'key'],
    ['今日 Token', tokens, '', '本代理累计 · UTC 日界', 'activity'],
  ];
  $('summary').innerHTML = metrics.map(([label, value, unit, caption, symbol]) => `<div class="metric"><div class="metric-label">${label}${icon(symbol)}</div><div class="metric-value">${number(value)}<small>${unit}</small></div><div class="metric-caption">${caption}</div></div>`).join('');
  $('nav-count').textContent = state.backends.length;
  $('provider-count').textContent = state.backends.length;
  $('app-version').textContent = `v${state.runtime?.version || '—'}`;
  $('overview-updated').textContent = state.updatedAt ? `本地数据更新于 ${state.updatedAt.toLocaleTimeString('zh-CN', { hour12: false })} · 额度可在服务卡片中单独更新` : '';
  const url = location.origin + '/v1';
  $('overview-endpoint').textContent = url;
  $('settings-endpoint').textContent = url;
  $('api-auth-hint').textContent = state.runtime?.api_key_required ? 'API 鉴权已开启。请求需携带调用密钥；管理密钥用于控制台。' : '当前本地 API 未要求调用密钥。应用必填时可使用任意非空值。';
  fillModelSelectors();
  renderProviders();
  renderModels();
  renderRequests();
  renderTasks();
  renderExample();
  renderGateway();
  window.SelectControls?.refresh();
}
/** 用服务名称、账号和额度构造服务卡片。 */
function providerCard(b) {
  const accounts = accountsOf(b.id);
  const active = accounts.filter((a) => a.enabled && a.has_credential);
  const q = quotaOf(b);
  const quotaRows = (state.quota || []).filter((r) => r.backend_id === b.id);
  const health = state.health.find((h) => h.backend_id === b.id);
  let status = !b.enabled ? badge('已停用') : isModelScope(b) && !b.has_api_key ? badge('待配置令牌', 'warn') : health?.cooling ? badge('路由冷却', 'warn') : isPrivate(b) && !active.length ? badge('待授权', 'warn') : badge('已启用', 'good');
  const modelList = modelsOf(b);
  const used = state.usage.find((u) => u.backend_id === b.id)?.used_tokens;
  const remaining = q ? q.remaining ?? Math.max(0, q.total - q.used) : null;
  const percent = q?.total > 0 ? Math.min(100, Math.max(0, remaining / q.total * 100)) : 0;
  const quotaLabel = hasAccountQuota(b) ? q ? q.remaining != null ? `余额 ${quotaNumber(q.remaining)} ${esc(q.unit)}` : `剩余 ${number(remaining)} / ${number(q.total)} ${esc(q.unit)}` : quotaRows.length ? '部分额度未知' : isModelScope(b) ? '剩余额度未知 · 可读取魔粒余额' : '额度尚未查询' : costs[b.cost_tier] || '费用未知';
  return `<article class="provider-card">
    <div class="provider-card-top"><span class="provider-logo ${esc(b.kind)}" aria-hidden="true">${b.kind === 'qoder' ? 'Q' : b.kind === 'workbuddy' ? 'w' : icon('layers')}</span><div class="provider-name"><h3>${esc(b.name)}</h3><p>${isPrivate(b) ? '账号授权' : 'OpenAI 兼容网关'} · ${esc(costs[b.cost_tier] || '费用未知')}</p></div>${status}<button class="icon-button" data-action="provider" data-id="${esc(b.id)}" aria-label="管理 ${esc(b.name)}">${icon('more')}</button></div>
    <div class="provider-body"><div class="provider-stats"><div><div class="stat-label">配置模型</div><div class="stat-value">${modelList.length}<small>个</small></div></div><div><div class="stat-label">今日 Token</div><div class="stat-value">${number(used)}<small>${used == null ? '暂无记录' : 'tokens'}</small></div></div></div>
    <div class="quota-mini"><div class="quota-line"><span>${quotaLabel}</span>${hasAccountQuota(b) ? `<button class="text-button" data-action="quota" aria-label="更新 ${esc(b.name)} 账号额度">${q ? '更新' : '读取额度'}</button>` : `<span>${b.has_api_key ? '密钥已配置' : '未设置密钥'}</span>`}</div>${q && q.total != null ? `<div class="progress" aria-label="剩余额度 ${percent.toFixed(0)}%"><span style="width:${percent}%"></span></div>` : ''}</div>
    <div class="provider-models">${modelList.slice(0, 3).map((m) => `<span class="model-chip" title="${esc(modelTitle(b, m) + ' · ' + m)}">${esc(modelTitle(b, m))}</span>`).join('')}${modelList.length > 3 ? `<button class="model-chip" data-action="provider-models" data-id="${esc(b.id)}">+${modelList.length - 3}</button>` : ''}${!modelList.length ? '<span class="muted">登录或同步后显示模型目录</span>' : ''}</div></div>
    <div class="provider-footer"><div class="account-line"><span class="account-avatar">${esc(accounts[0]?.name?.slice(0, 1).toUpperCase() || (isPrivate(b) ? '—' : 'K'))}</span><span>${esc(accounts.length ? accounts[0].name + (accounts.length > 1 ? ` +${accounts.length - 1}` : '') : isPrivate(b) ? '尚未添加账号' : 'API Key 接入')}</span></div><div class="button-row">${b.enabled && modelList.length ? `<button class="text-button" data-action="try-provider" data-id="${esc(b.id)}">试聊 ${icon('arrow')}</button>` : isPrivate(b) && b.enabled ? `<button class="text-button" data-action="login" data-id="${esc(b.id)}">登录账号 ${icon('arrow')}</button>` : ''}<button class="button small" data-action="provider" data-id="${esc(b.id)}">管理</button></div></div></article>`;
}
/** 同时刷新概览卡片和服务页的搜索结果。 */
function renderProviders() {
  renderQuotaStatus();
  const blank = empty('还没有接入服务', '添加 WorkBuddy、Qoder，或一个 OpenAI 兼容网关。', '<button class="button primary" data-action="add-provider">添加第一个服务</button>');
  $('overview-providers').innerHTML = state.backends.length ? state.backends.map(providerCard).join('') : blank;
  const search = $('provider-search').value.toLowerCase();
  const filter = $('provider-filter').value;
  const rows = state.backends.filter((b) => `${b.name} ${b.kind}`.toLowerCase().includes(search) && (filter === 'all' || b.enabled === (filter === 'enabled')));
  $('providers-result').textContent = `${rows.length} / ${state.backends.length} 个服务`;
  $('all-providers').innerHTML = rows.length ? rows.map(providerCard).join('') : state.backends.length ? empty('没有匹配的服务', '试试其他名称，或更改状态筛选。') : blank;
}
/** 更新模型下拉时保留用户当前选择。 */
function fillModelSelectors() {
  const options = ['模型网关', '逆向 API'].map((category) => state.backends.filter((b) => b.enabled && providerCategory(b) === category).map((b) => `<optgroup label="${esc(b.name)}" data-category="${category}" data-group-id="${esc(b.id)}" data-collapsed="true">${modelsOf(b).map((m) => `<option value="${esc(modelCallName(b, m))}" data-title="${esc(modelName(b, m))}" data-description="${esc(b.name + ' · ' + modelCallName(b, m))}" data-badge="${esc(modelRate(b, m))}" data-original-badge="${esc(modelOriginalRate(b, m))}" data-badge-title="上游积分消耗倍率">${esc(b.name + ' / ' + modelTitle(b, m))}</option>`).join('')}</optgroup>`).join('')).join('');
  for (const id of ['play-model', 'example-model']) {
    const current = $(id).value;
    const selected = modelAt(current);
    $(id).innerHTML = autoOptions() + options;
    const next = selected ? modelCallName(selected.backend, selected.id) : current.startsWith('auto:') ? 'auto' : current;
    if ([...$(id).options].some((o) => o.value === next)) $(id).value = next;
  }
  const current = $('model-provider').value;
  $('model-provider').innerHTML = '<option value="">全部服务</option>' + state.backends.map((b) => `<option value="${esc(b.id)}">${esc(b.name)}${b.enabled ? '' : '（已停用）'}</option>`).join('');
  if (state.backends.some((b) => b.id === current)) $('model-provider').value = current;
  renderSelectedLimits('play-model', 'play-limits');
  renderSelectedLimits('example-model', 'example-limits');
}
/** 仅保存非敏感的分组折叠偏好；存储不可用时保留内存状态。 */
function loadModelGroups() {
  if (!state.modelGroups) {
    try { state.modelGroups = JSON.parse(localStorage.getItem('fmp-model-groups') || '{}'); } catch { state.modelGroups = {}; }
    if (!state.modelGroups || typeof state.modelGroups !== 'object' || Array.isArray(state.modelGroups)) state.modelGroups = {};
  }
  return state.modelGroups;
}
/** 折叠偏好在搜索期间保持不变，清除搜索后恢复。 */
function saveModelGroup(id, open) {
  loadModelGroups()[id] = open;
  try { localStorage.setItem('fmp-model-groups', JSON.stringify(state.modelGroups)); } catch { /* 页面内状态仍有效。 */ }
}
/** 批量控制当前服务分组，并记住偏好。 */
function expandModelGroups(open) {
  for (const group of $('model-list').querySelectorAll('.model-group')) {
    saveModelGroup(group.dataset.backend, open);
    group.open = open;
  }
}
/** 展示按服务折叠的模型目录；搜索自动展开匹配组。 */
function renderModels() {
  const search = $('model-search').value.trim().toLowerCase();
  const provider = $('model-provider').value;
  const groups = state.backends.filter((b) => !provider || b.id === provider).map((b) => ({ b, models: modelsOf(b).filter((m) => `${modelName(b, m)} ${m} ${modelCallName(b, m)} ${b.name}`.toLowerCase().includes(search)) })).filter((group) => group.models.length);
  const count = groups.reduce((total, group) => total + group.models.length, 0);
  $('models-count').textContent = `${count} / ${state.backends.reduce((total, b) => total + modelsOf(b).length, 0)}`;
  const preferences = loadModelGroups();
  $('model-list').innerHTML = groups.length ? groups.map(({ b, models }) => {
    const open = search || (Object.hasOwn(preferences, b.id) ? preferences[b.id] : b.id === state.backends[0]?.id);
    return `<details class="model-group" data-backend="${esc(b.id)}" ${open ? 'open' : ''}><summary><span>${esc(b.name)}</span><span class="count">${models.length}${search ? ' 个匹配' : ' 个模型'}</span>${b.enabled ? '' : badge('已停用', 'warn')}</summary><div>${models.map((m) => {
      const name = modelCallName(b, m);
      return `<div class="model-row"><button class="model-item ${$('play-model').value === name ? 'selected' : ''}" data-action="select-model" data-id="${esc(name)}" ${b.enabled ? '' : 'disabled'}><span class="model-info"><strong>${esc(modelName(b, m))}</strong><small class="model-call-name">${esc(name)}</small><small class="model-limit">${esc(modelSummary(b, m))}</small></span>${modelRateBadge(b, m)}</button><button class="text-button model-params" data-action="model-detail" data-id="${esc(name)}" aria-label="查看 ${esc(modelName(b, m))} 参数">参数</button></div>`;
    }).join('')}</div></details>`;
  }).join('') : search || provider
    ? empty('没有匹配的模型', '清除搜索和服务筛选后查看全部模型。', '<button class="button" data-action="reset-models">重置筛选</button>')
    : state.backends.length
      ? empty('还没有模型目录', '在服务详情中登录账号或同步模型目录。', '<a class="button" href="#providers">管理服务</a>')
      : empty('还没有接入模型', '先添加一个模型网关或逆向 API 服务。', '<button class="button primary" data-action="add-provider">添加服务</button>');
  for (const group of $('model-list').querySelectorAll('.model-group')) {
    let previousOpen = group.open;
    group.addEventListener('toggle', () => {
      if (group.isConnected && group.open !== previousOpen && !$('model-search').value.trim()) saveModelGroup(group.dataset.backend, group.open);
      previousOpen = group.open;
    });
  }
  renderSelectedLimits('play-model', 'play-limits');
}
/** 选中模型并切换至调试页。 */
function selectModel(id) {
  const model = modelAt(id);
  if (model) id = modelCallName(model.backend, model.id);
  if (state.chat) { toast('请先停止当前请求再切换模型'); return; }
  if (![...$('play-model').options].some((o) => o.value === id)) { toast('此模型所属服务尚未启用', true); return; }
  $('play-model').value = id;
  if (model) {
    saveModelGroup(model.backend.id, true);
    $('model-search').value = '';
    $('model-provider').value = '';
  }
  window.SelectControls?.refresh();
  $('drawer').close();
  location.hash = 'models';
  renderModels();
}
/** 请求生命周期用固定标签表示，HTTP 状态单独显示。 */
function requestOutcome(record) {
  return { ok: ['已完成', 'good'], running: ['进行中', 'warn'], error: ['失败', 'bad'], cancelled: ['已取消', 'warn'], interrupted: ['进程中断', 'warn'] }[record.outcome] || ['未知', 'warn'];
}
/** 持久记录必须显示日期，避免重启或跨天后把同一时刻混淆。 */
function requestTime(value) {
  return value ? new Date(value * 1000).toLocaleString('zh-CN', { hour12: false }) : '—';
}
/** 按证据区分原生与推断会话，旧记录保持未声明状态。 */
function recordSessionTitle(record) {
  const client = { trae: 'TRAE', minimax_code: 'MiniMax Code' }[record.client || record.session?.client];
  const kind = record.session?.confidence === 'inferred' ? '推断会话' : record.session?.source === 'mavis_header' ? '原生会话' : '会话';
  return `${client ? client + ' · ' : ''}${kind}`;
}
/** 用可读来源解释分组的可信边界。 */
function recordSessionEvidence(record) {
  return {
    fmp_header: '来自明确的 FMP 会话头',
    mavis_header: '来自 MiniMax Code 原生会话 ID',
    history_new: '推断起点：尚未匹配已完成历史',
    history_continuation: '推断延续：匹配完整历史与 FMP 已完成输出',
    history_tool_anchor: '推断延续：裁剪历史保留唯一完整工具调用与结果关联',
    history_ambiguous: '历史匹配多个会话，已拆为新组',
  }[record.session?.source] || '旧记录未保存关联来源';
}
/** 渲染会话证据与限制，所有客户端字段都按文本转义。 */
function recordSessionDetails(record) {
  if (!record.session_id) return '';
  const evidence = record.session?.evidence;
  const previous = record.session?.previous_request_id;
  return `<p class="hint">${esc(recordSessionTitle(record))}</p><pre>${esc(record.session_id)}</pre><p class="hint">${esc(recordSessionEvidence(record))}${record.session?.client_session_id ? ` · 客户端 ID：${esc(record.session.client_session_id)}` : ''}${previous ? ` · 匹配请求：${esc(previous)}` : ''}</p>${evidence ? `<p class="hint">逻辑历史 ${number(evidence.logical_messages)} 条 · 匹配 ${number(evidence.matched_messages)} 条 · 候选会话 ${number(evidence.candidate_groups)} 个${evidence.matched_tool_calls ? ` · 工具锚点 ${number(evidence.matched_tool_calls)} 个调用` : ''}${evidence.normalized_tool_ids ? ` · 已归一 ${number(evidence.normalized_tool_ids)} 个 TRAE 工具调用 ID，保留名称、参数与结果配对` : ''}</p>` : ''}${previous && state.records.some((item) => item.id === previous) ? `<button class="text-button" data-action="request-detail" data-id="${esc(previous)}">查看匹配的前一请求</button>` : ''}${record.session?.confidence === 'inferred' ? '<p class="hint">此分组由请求历史推断，不代表客户端原生 SessionID。用于请求关联与会话调度绑定。历史压缩、截断或 FMP 重启后可能拆组；复制完整历史也可能归入同组。</p>' : ''}`;
}
/** 精确切换 trace 或会话分组，避免把搜索子串当作关联关系。 */
function requestGroup(kind, value) {
  state.recordGroup = kind && value ? { kind, value } : null;
  $('request-search').value = '';
  $('request-filter').value = '';
  $('request-protocol').value = '';
  $('modal').close();
  location.hash = 'requests';
  renderRequests();
  window.SelectControls?.refresh();
}
/** 渲染最近请求、保存状态与可筛选的完整记录。 */
function renderRequests() {
  const storage = state.recordStorage;
  $('record-storage').className = `notice${storage?.last_error || storage?.enabled === false ? ' warning' : ''}`;
  $('record-storage').textContent = !storage ? '正在读取保存状态…' : storage.last_error ? `文件保存失败：${storage.last_error}。当前展示可能包含尚未落盘的记录。` : storage.enabled ? `已保存到 ${storage.path} · 保留最近 ${number(storage.capacity)} 条 · FMP_RECORD_CAPACITY 可调整，重启生效。${storage.recovered_invalid_lines ? ` 启动时跳过 ${storage.recovered_invalid_lines} 条损坏记录。` : ''}` : '当前实例未启用文件保存。';
  $('record-group').hidden = !state.recordGroup;
  const groupedRecord = state.records.find((record) => record.session_id === state.recordGroup?.value);
  $('record-group').innerHTML = state.recordGroup ? `${state.recordGroup.kind === 'trace_id' ? 'Trace' : esc(recordSessionTitle(groupedRecord || {}))}：<code>${esc(state.recordGroup.value)}</code> <button class="text-button" data-action="request-ungroup">查看全部</button>` : '';
  $('recent-requests').innerHTML = state.records.length ? state.records.slice(0, 4).map((r) => `<div class="recent-row"><span class="request-symbol">${r.outcome === 'ok' ? '↗' : '!'}</span><div class="recent-title"><strong>${esc(modelLabel(r.resolved_model || r.requested_model) || '未解析模型')}</strong><p>${esc(r.protocol)} · ${requestOutcome(r)[0]} · HTTP ${r.status || '待定'}</p></div><span class="recent-time">${time(r.at)}</span><button class="icon-button" data-action="request-detail" data-id="${esc(r.id)}" aria-label="查看请求 ${esc(r.id)}">${icon('arrow')}</button></div>`).join('') : empty('还没有请求记录', '从模型与调试页面发起第一次调用。', '<a class="text-link" href="#models">开始试聊 ↗</a>');
  const search = $('request-search').value.toLowerCase();
  const filter = $('request-filter').value;
  const protocol = $('request-protocol').value;
  const rows = state.records.filter((r) => `${modelLabel(r.resolved_model || r.requested_model)} ${r.requested_model} ${r.resolved_model} ${r.id} ${r.trace_id || ''} ${r.session_id || ''} ${r.client || ''} ${recordSessionTitle(r)}`.toLowerCase().includes(search) && (!filter || r.outcome === filter) && (!protocol || r.protocol === protocol) && (!state.recordGroup || r[state.recordGroup.kind] === state.recordGroup.value));
  $('requests-result').textContent = `${rows.length} / ${state.records.length} 条记录`;
  $('requests-table').innerHTML = rows.length ? `<table><thead><tr><th>时间</th><th>模型 / 服务</th><th>协议</th><th>结果</th><th>总耗时</th><th></th></tr></thead><tbody>${rows.map((r) => `<tr><td class="mono nowrap">${requestTime(r.at)}</td><td><div class="model-name">${esc(modelLabel(r.resolved_model || r.requested_model) || '未解析模型')}</div><div class="secondary">${esc(r.id.slice(0, 16))}…</div>${r.trace_id ? `<button class="text-button mono" data-action="request-trace" data-id="${esc(r.id)}">Trace ${esc(r.trace_id.slice(0, 12))}…</button>` : ''}${r.session_id ? `<div><button class="text-button" data-action="request-session" data-id="${esc(r.id)}" title="${esc(recordSessionEvidence(r))}">${esc(recordSessionTitle(r))} ${esc(r.session_id)}</button></div>` : r.client ? `<div class="secondary">${esc({trae: 'TRAE', minimax_code: 'MiniMax Code'}[r.client] || r.client)} · 未关联会话</div>` : ''}</td><td>${esc(r.protocol)}<div class="secondary">${r.stream ? '流式' : '非流式'}</div></td><td>${badge(...requestOutcome(r))}<div class="secondary">HTTP ${r.status || '待定'}</div></td><td class="mono nowrap">${r.outcome === 'running' ? '进行中' : `${number(r.duration_ms)} ms`}</td><td><button class="text-button" data-action="request-detail" data-id="${esc(r.id)}">详情</button></td></tr>`).join('')}</tbody></table>` : empty('暂无匹配记录', state.records.length ? '更改搜索或筛选条件。' : '请求元数据将在这里显示，并在重启后恢复。');
}
/** 接受最新配置版本，避免较早发起的刷新覆盖刚保存的配置。 */
function acceptSchedule(schedule) {
  if (!state.schedule || schedule.settings.revision >= state.schedule.settings.revision) state.schedule = schedule;
}
/** 读取编辑值，保存和异步返回时用同一份字段判断是否有新输入。 */
function scheduleInput() {
  return { enabled: $('schedule-enabled').checked, time: $('schedule-time').value, random_minutes: Number($('schedule-random').value) };
}
/** 解释随机区间并限制结束时间在当日；0 分钟为固定时间。 */
function renderScheduleWindow() {
  const { time, random_minutes: minutes } = scheduleInput();
  const [hour, minute] = time.split(':').map(Number);
  const start = hour * 60 + minute;
  const max = 1440 - start;
  $('schedule-random').max = Number.isFinite(max) ? max : 1440;
  const valid = /^\d{2}:\d{2}$/.test(time) && hour < 24 && minute < 60 && Number.isInteger(minutes) && minutes >= 0 && minutes <= max;
  const end = start + minutes;
  const endTime = `${String(Math.floor(end / 60)).padStart(2, '0')}:${String(end % 60).padStart(2, '0')}`;
  $('schedule-window').textContent = !valid ? '请输入当日有效时间和随机分钟数，结束时间不能超过 24:00。' : minutes === 0 ? `每天 ${time} 固定执行；均为北京时间。` : `每天在 ${time}–${endTime} 内，为每个账号独立抽取一次执行时间。窗口内开启时从剩余时间抽取。`;
  $('schedule-save-status').textContent = state.scheduleDirty ? '有未保存的修改' : state.schedule ? '设置已持久保存' : '正在读取设置…';
  $('schedule-save').disabled = state.scheduleSaving || !state.schedule || !state.scheduleDirty;
}
/** 同步持久配置时保留用户尚未保存的编辑内容。 */
function renderScheduleForm() {
  const settings = state.schedule?.settings;
  if (settings && !state.scheduleDirty) {
    $('schedule-enabled').checked = settings.enabled;
    $('schedule-time').value = settings.time;
    $('schedule-random').value = settings.random_minutes;
  }
  renderScheduleWindow();
}
/** 保存到服务端数据库，异步请求期间的新编辑继续保留为草稿。 */
async function saveSchedule(event) {
  event.preventDefault();
  if (state.scheduleSaving || !$('schedule-form').reportValidity()) return;
  const input = scheduleInput();
  state.scheduleSaving = true;
  try {
    await busy(event.submitter, async () => {
      const schedule = await api('/api/tasks/checkin-schedule', { method: 'PUT', body: input });
      acceptSchedule(schedule);
      state.scheduleDirty = JSON.stringify(scheduleInput()) !== JSON.stringify(input);
      renderTasks();
      toast(input.enabled ? '自动签到设置已保存并开启' : '自动签到设置已保存');
    });
  } finally {
    state.scheduleSaving = false;
    renderScheduleWindow();
  }
}
/** 按指定时区展示计划时间，不依赖浏览器所在时区。 */
function scheduleTime(seconds) {
  return new Date(seconds * 1000).toLocaleString('zh-CN', {
    timeZone: 'Asia/Shanghai', month: '2-digit', day: '2-digit',
    hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false,
  });
}
/** 按服务展示每日动作，积分保留不能显示成新奖励领取。 */
function dailyTaskLabel(backend) {
  return backend?.kind === 'raccoon' ? '登录保留积分' : isModelScope(backend) ? '登录领取魔粒' : '签到';
}
/** 展示实际持久配置、下一次执行计划和服务端注册的签到执行器。 */
function renderTasks() {
  renderOperationHistory();
  renderBatch();
  renderScheduleForm();
  const schedule = state.schedule;
  const enabled = schedule?.settings.enabled;
  $('scheduler-status').textContent = schedule?.last_error ? `调度暂遇错误：${schedule.last_error}` : enabled == null ? '调度状态尚未读取' : enabled && !schedule.running ? '自动签到已开启，但调度器未运行，请检查服务日志。' : enabled ? '自动签到已开启 · 每个账号每日尝试一次；执行时间允许约 1 分钟调度误差。' : '自动签到已关闭 · 下方时间为计划预览，可随时手动签到。';
  $('scheduler-status').className = `notice${enabled && schedule?.running && !schedule?.last_error ? '' : ' warning'}`;
  const providers = state.backends.filter((b) => state.runtime?.checkin_backends.includes(b.id));
  $('task-providers').innerHTML = providers.length ? providers.map((b) => {
    const accounts = accountsOf(b.id).filter((a) => a.enabled);
    return `<div class="task-card"><h3>${esc(b.name)}</h3><p>${accounts.length} 个启用账号 · 每日${dailyTaskLabel(b)}</p>${b.kind === 'raccoon' ? '<p class="hint">核对当天积分到期时间；结果未知时请打开官网工作台确认。</p>' : b.kind === 'trae_personal' ? '<p class="hint">先查询个人账号今日状态，再领取积分；结果未知时请在个人客户端核对。</p>' : ''}<div class="schedule-runs">${accounts.map((a) => {
      const next = schedule?.next_runs.find((plan) => plan.backend_id === b.id && plan.account_id === a.id);
      return `<div><span>${esc(a.name)}</span><span class="mono">${next ? `${esc(scheduleTime(next.scheduled_at))}${next.status === 'running' ? ' · 执行中' : ''}` : '暂无计划'}</span></div>`;
    }).join('')}</div><button class="button small" data-action="checkin" data-id="${esc(b.id)}" ${accounts.length && !state.batchStarting && state.batch?.run?.status !== 'running' ? '' : 'disabled'}>执行${dailyTaskLabel(b)}</button>${b.kind === 'raccoon' ? ' <a class="button small" href="https://xiaohuanxiong.com/work?lang=zh" target="_blank" rel="noopener noreferrer">打开官网 ↗</a>' : ''}</div>`;
  }).join('') : empty('暂无可执行任务', '已启用的账号服务会在这里显示支持的任务。');
  const statuses = { succeeded: ['执行成功', 'good'], already_done: ['已完成', 'good'], failed: ['执行失败', 'bad'], unknown: ['结果未知', 'warn'], skipped: ['已跳过', ''] };
  $('task-count').textContent = `${state.tasks.length} 条`;
  $('tasks-table').innerHTML = state.tasks.length ? `<table><thead><tr><th>周期</th><th>服务 / 账号</th><th>任务</th><th>状态</th><th>结果</th></tr></thead><tbody>${state.tasks.map((t) => `<tr><td class="mono nowrap">${esc(t.period)}</td><td>${esc(backendName(t.backend_id))}<div class="secondary">${esc(state.accounts.find((a) => a.id === t.account_id)?.name || t.account_id)}</div></td><td>${t.kind === 'checkin' ? dailyTaskLabel(state.backends.find((b) => b.id === t.backend_id)) : esc(t.kind)}</td><td>${badge(...(statuses[t.status] || ['未知', 'warn']))}</td><td>${esc(t.message)}${t.amount != null ? `<div class="secondary">记录奖励 ${number(t.amount)}</div>` : ''}</td></tr>`).join('')}</tbody></table>` : empty('还没有任务执行记录', '这里展示代理的执行记录，客户端内的签到不会自动同步到本地记录。');
}
/** 操作历史读取持久状态，查看按钮恢复操作而不重新开始。 */
function renderOperationHistory() {
  const target = $('operation-history');
  if (!target) return;
  target.innerHTML = state.operations.length
    ? `<table><thead><tr><th>服务</th><th>操作</th><th>状态</th><th></th></tr></thead><tbody>${state.operations.map((operation) => `<tr><td>${esc(backendName(operation.service_id))}</td><td>${esc(operation.action)}</td><td>${esc(operation.status)}</td><td><button class="text-button" data-action="operation" data-id="${esc(operation.operation_id)}">查看</button></td></tr>`).join('')}</tbody></table>`
    : empty('暂无管理操作', '登录、模型同步和额度操作在此保留可恢复的状态。');
}
/** 防止较早的查询覆盖已经完成的账号进度；后台轮询可确认服务重启后的空状态。 */
function acceptBatch(batch, allowEmpty = false) {
  const current = state.batch?.run;
  const next = batch.run;
  if (!next && current?.status === 'running' && !allowEmpty) return;
  if (next && current && (next.started_at < current.started_at || next.id === current.id && next.version < current.version)) return;
  state.batch = batch;
}
/** 展示批次进度与真实领取结果，签到期间禁用重复启动按钮。 */
function renderBatch() {
  const run = state.batch?.run;
  const running = run?.status === 'running';
  const done = run?.accounts.filter((account) => !['pending', 'waiting', 'running'].includes(account.status)).length || 0;
  const eligible = state.accounts.some((account) => account.enabled && state.runtime?.checkin_backends?.includes(account.backend_id));
  const button = $('checkin-all');
  button.disabled = state.batchStarting || running || !eligible;
  button.textContent = state.batchStarting ? '正在启动…' : running ? `正在签到 ${done}/${run.accounts.length}` : '一键签到所有账号';
  if (state.batchStarting || running) button.setAttribute('aria-busy', 'true'); else button.removeAttribute('aria-busy');
  $('batch-checkin-panel').hidden = !run;
  if (!run) return;
  $('batch-progress').textContent = `${running ? '进行中' : '已完成'} · ${done}/${run.accounts.length}`;
  $('batch-error').hidden = !state.batchError && !run.last_error;
  $('batch-error').textContent = state.batchError || run.last_error || '';
  const statuses = { pending: ['等待执行', ''], waiting: ['随机等待', 'warn'], running: ['正在执行', 'warn'], succeeded: ['执行成功', 'good'], already_done: ['已领取', 'good'], failed: ['执行失败', 'bad'], unknown: ['结果未知', 'warn'], skipped: ['已跳过', ''] };
  $('batch-results').innerHTML = `<table><thead><tr><th>服务 / 账号</th><th>状态</th><th>结果</th></tr></thead><tbody>${run.accounts.map((account) => `<tr><td>${esc(account.backend_name)}<div class="secondary">${esc(account.account_name)}</div></td><td>${badge(...(statuses[account.status] || ['未知', 'warn']))}${account.delay_ms != null ? `<div class="secondary">随机间隔 ${(account.delay_ms / 1000).toFixed(1)} 秒</div>` : ''}</td><td>${esc(account.message)}${account.amount != null ? `<div class="secondary">记录奖励 ${number(account.amount)}</div>` : ''}</td></tr>`).join('')}</tbody></table>`;
}
/** 短请求启动后台批次，关闭页面或查询超时不会重新提交领取动作。 */
async function checkinAll(button) {
  if (state.batchStarting || state.batch?.run?.status === 'running') return;
  state.batchStarting = true;
  try {
    await busy(button, async () => {
      const batch = await api('/api/tasks/checkin-all', { method: 'POST' });
      acceptBatch(batch);
      state.batchError = '';
      toast('已开始一键签到');
    });
  } finally {
    state.batchStarting = false;
    renderTasks();
    scheduleBatchPoll();
  }
}
/** 仅查询运行中的批次，完成后刷新任务记录；失败只重读进度，不重新签到。 */
function scheduleBatchPoll() {
  clearTimeout(state.batchTimer);
  state.batchTimer = null;
  if (state.batch?.run?.status !== 'running') return;
  state.batchTimer = setTimeout(async () => {
    state.batchTimer = null;
    try {
      acceptBatch(await api('/api/tasks/checkin-all'), true);
      state.batchError = '';
      renderTasks();
      if (state.batch?.run?.status !== 'running') await refresh();
    } catch (error) {
      state.batchError = `暂时无法读取进度：${error.message} 后台签到会继续，可刷新记录查看结果。`;
      renderBatch();
    }
    scheduleBatchPoll();
  }, 2000);
}
/** 打开右侧面板并明确关联标题。 */
function drawer(title, eyebrow, content) {
  window.SelectControls?.close();
  $('drawer-title').textContent = title;
  $('drawer-eyebrow').textContent = eyebrow;
  $('drawer-body').innerHTML = content;
  if (!$('drawer').open) $('drawer').showModal();
  window.SelectControls?.refresh();
}
/** 打开通用操作对话框。 */
function modal(title, content) {
  $('modal-body').dataset.operationId = '';
  window.SelectControls?.close();
  $('modal-title').textContent = title;
  $('modal-body').innerHTML = content;
  if (!$('modal').open) $('modal').showModal();
  window.SelectControls?.refresh();
}
/** 展示服务详情，登录、同步、刷新、编辑均在所属服务内完成。 */
function openProvider(id, resetScroll = true) {
  const b = state.backends.find((item) => item.id === id);
  if (!b) return;
  state.drawerProvider = id;
  const oldScroll = $('drawer').scrollTop;
  const accounts = accountsOf(id);
  const health = state.health.find((h) => h.backend_id === id);
  const models = modelsOf(b);
  drawer(b.name, 'PROVIDER DETAILS', `
    <div class="button-row">${badge(b.enabled ? '已启用' : '已停用', b.enabled ? 'good' : '')}${badge(costs[b.cost_tier] || '费用未知')}<button class="text-button" data-action="edit-provider" data-id="${esc(id)}">编辑配置</button></div>
    <div class="button-row">${(providerPlugin(b)?.operations || []).filter((action) => !['auth.start', 'models.sync', 'credential.refresh'].includes(action.id)).map((action) => `<button class="button small" data-action="plugin-operation" data-operation="${esc(action.id)}" data-id="${esc(id)}" ${b.enabled ? '' : 'disabled'}>${esc(action.title)}</button>`).join('')}</div>
    <div class="detail-id">${esc(b.base_url)}</div><details class="drawer-section"><summary>技术标识</summary><pre>${esc(id)}</pre><p class="hint">客户端使用模型调用名称；此标识用于内部定位。</p></details>
    <dl class="detail-summary"><div><dt>路由健康</dt><dd>${health ? health.cooling ? `冷却 ${health.cooldown_remaining_secs}s` : '最近路由已响应' : '暂无健康样本'}</dd></div><div><dt>路由建流延迟</dt><dd>${health?.latency_ms == null ? '—' : number(health.latency_ms) + ' ms'}</dd></div><div><dt>优先级</dt><dd>${b.priority}</dd></div><div><dt>配置日预算</dt><dd>${b.daily_token_budget ? number(b.daily_token_budget) + ' Token' : '未限制'}</dd></div></dl>
    <div class="drawer-section"><div class="section-heading"><h3>${isPrivate(b) ? '授权账号' : 'API 凭证'}</h3>${isPrivate(b) ? `<button class="button small soft" data-action="login" data-id="${esc(id)}" ${b.enabled ? '' : 'disabled'}>${icon('plus')}登录账号</button>` : ''}</div>
    ${isModelScope(b) ? modelScopeGuidance() : ''}
    ${isModelScope(b) ? accounts.map((a) => {
      const quota = state.quota?.find((q) => q.account_id === a.id && q.backend_id === id);
      const balance = quota?.routing?.remaining ?? quota?.remaining;
      return `<div class="detail-account"><strong>${esc(a.name)}</strong><p>${quota?.confidence === 'verified' && balance != null ? `可用余额 ${quotaNumber(balance)} 魔粒 · ${time(quota.checked_at)} 查询` : esc(quota?.note || '额度尚未查询')}</p>${quota?.note && balance != null ? `<p class="hint">${esc(quota.note)}</p>` : ''}<button class="button small" data-action="quota">更新魔粒余额</button><button class="button small" data-action="checkin" data-id="${esc(id)}" ${b.enabled ? '' : 'disabled'}>每日登录领取魔粒</button></div>`;
    }).join('') : ''}
    ${b.kind === 'raccoon' ? '<p class="hint">每日 00:00 自动发放积分；当天登录可保留当天这笔积分至含发放日的第七天。任务页支持每日登录并核对实际到期时间。</p><a class="button small" href="https://xiaohuanxiong.com/work?lang=zh" target="_blank" rel="noopener noreferrer">打开官网保留积分 ↗</a>' : ''}
    ${b.kind === 'trae_personal' ? `<p class="hint">使用 TRAE CN 个人账号完成官网授权，账号保存后自动同步模型。网页登录需与代理运行在同一台电脑。</p><div class="button-row"><button class="button small" data-action="checkin" data-id="${esc(id)}" ${b.enabled && accounts.some((a) => a.enabled) ? '' : 'disabled'}>每日签到</button></div><p class="hint">任务页支持每日自动签到，先查询当天状态再领取积分。额度读取官方通用积分余额，保留小数；模型扣费规则尚未验证。</p>` : ''}
    ${isPrivate(b) ? accounts.length ? accounts.map((a) => {
      const quota = state.quota?.find((q) => q.account_id === a.id && q.backend_id === id);
      const balance = ['monkeycode', 'raccoon', 'trae_personal'].includes(b.kind) ? quota?.routing?.remaining ?? quota?.remaining : quota?.remaining;
      return `<div class="detail-account"><div class="detail-account-top"><strong class="account-label">${esc(a.name)}</strong>${badge(a.enabled ? a.has_credential ? '凭证已保存' : '缺少凭证' : '已停用', a.enabled && a.has_credential ? 'good' : 'warn')}</div><p class="hint">${quota ? balance != null ? `余额 ${quotaNumber(balance)} ${esc(quota.unit || '')} · ${time(quota.checked_at)} 查询` : quota.total != null && quota.used != null ? `剩余 ${number(Math.max(0, quota.total - quota.used))} / ${number(quota.total)} ${esc(quota.unit || '')} · ${time(quota.checked_at)} 查询` : esc(quota.note || '额度未知') : '额度尚未查询'}</p>${b.kind === 'monkeycode' && quota?.note ? `<p class="hint">${esc(quota.note)}</p>` : ''}<div class="button-row">${b.kind === 'monkeycode' ? `<button class="button small" data-action="login" data-id="${esc(id)}">重新登录</button>` : `<button class="button small" data-action="refresh-account" data-id="${esc(a.id)}">刷新凭证</button>`}<button class="button small" data-action="edit-account" data-id="${esc(a.id)}">编辑账号</button><button class="text-button" data-action="delete-account" data-id="${esc(a.id)}">移除</button></div></div>`;
    }).join('') : empty('尚无账号', b.kind === 'monkeycode' ? '点击登录账号，完成官方登录后自动保存并同步模型。' : '通过浏览器完成授权，账号会出现在这里。') : `<p class="muted">${b.has_api_key ? '密钥已保存 · ' + esc(b.api_key_masked) : '尚未设置 API Key。若上游需要鉴权，请编辑配置。'}</p>`}
    ${isPrivate(b) ? `<div class="button-row" style="margin-top:14px"><button class="text-button" data-action="add-account" data-id="${esc(id)}">手工导入凭证</button><button class="text-button" data-action="quota">更新额度</button></div>` : ''}</div>
    <div class="drawer-section"><div class="section-heading"><h3>模型目录 <span class="count">${models.length}</span></h3><button class="button small" data-action="sync-models" data-id="${esc(id)}" ${!b.enabled || isPrivate(b) && !accounts.some((a) => a.enabled && a.has_credential) ? 'disabled' : ''}>${icon('refresh')}同步目录</button></div><p class="hint" style="margin-bottom:12px">同步会保存模型能力、显示名、积分倍率和 Token 限制。活动开始或结束后请重新同步倍率。</p><details class="provider-catalog"><summary>查看 ${models.length} 个模型及参数</summary><div class="detail-models">${models.length ? models.map((m) => `<button class="model-chip detail-model-entry" data-action="model-detail" data-id="${esc(modelCallName(b, m))}"><span>${esc(modelName(b, m))}${modelRateBadge(b, m)}</span><small>${esc(modelCallName(b, m))}</small><small>${esc(modelSummary(b, m))}</small></button>`).join('') : '<span class="muted">同步后显示模型</span>'}</div></details></div>
    <div class="actions"><button class="button" data-action="toggle-provider" data-id="${esc(id)}">${b.enabled ? '停用服务' : isModelScope(b) && !b.has_api_key ? '配置令牌并启用' : '启用服务'}</button><button class="button danger" data-action="delete-provider" data-id="${esc(id)}">删除服务</button></div>`);
  $('drawer').scrollTop = resetScroll ? 0 : oldScroll;
  const actions = providerPlugin(b)?.operations || [];
  const supported = { login: 'auth.start', 'sync-models': 'models.sync', 'refresh-account': 'credential.refresh', checkin: 'checkin', quota: 'quota.refresh' };
  $('drawer-body').querySelectorAll('[data-action]').forEach((button) => {
    const required = supported[button.dataset.action];
    if (required && !actions.some((action) => action.id === required)) button.hidden = true;
    if (button.dataset.action === 'quota' && actions.some((action) => action.id === 'quota.refresh')) {
      button.dataset.action = 'plugin-operation'; button.dataset.operation = 'quota.refresh'; button.dataset.id = id;
    }
  });
}
/** 安全解析高级 JSON，并要求对象形态。 */
function jsonObject(raw, label) {
  if (!raw.trim()) return {};
  let data;
  try { data = JSON.parse(raw); } catch { throw new Error(`${label}必须是合法 JSON`); }
  if (!data || Array.isArray(data) || typeof data !== 'object') throw new Error(`${label}必须是 JSON 对象`);
  return data;
}
/** 打开新增或编辑服务表单，按类型收起无关字段。 */
function editProvider(id = '') {
  state.drawerProvider = null;
  const b = state.backends.find((item) => item.id === id);
  drawer(b ? `编辑 ${b.name}` : '添加模型服务', b ? 'EDIT PROVIDER' : 'CONNECT A PROVIDER', `
    <form id="backend-form"><input id="f-id" type="hidden" value="${esc(id)}">
    ${b ? '' : `<label>选择服务<select id="f-preset" data-collapsible="true">${providerPresetOptions()}</select></label><p class="hint">切换服务会重置下方配置。</p>`}
    <input id="f-kind" type="hidden" value="${esc(b?.kind || 'openai_compat')}"><p id="provider-kind-hint" class="hint"></p>
    <label>名称<input id="f-name" value="${esc(b?.name || '')}" required placeholder="为这个服务起个名字"></label>
    <div id="private-fields" hidden><div class="notice" id="private-login-hint">保存服务后，通过浏览器登录添加账号。</div></div>
    <label>Base URL<input id="f-url" type="url" placeholder="插件默认地址或 https://api.example.com/v1" value="${esc(b?.base_url || '')}"></label>
    <div id="provider-gateway-fields"><label>API Key<input id="f-key" type="password" autocomplete="off" placeholder="${b?.has_api_key ? '留空保留已保存的密钥' : '上游要求鉴权时填写'}"></label></div>
    <div id="plugin-fields" class="form-grid"></div>
    <div id="plugin-config-helpers"></div>
    <div id="modelscope-guidance" hidden>${modelScopeGuidance()}</div>
    <label>模型白名单<textarea id="f-models" rows="2" placeholder="用逗号分隔，例如 gpt-4.1,deepseek-chat">${esc(b?.models || '')}</textarea></label><p class="hint">账号服务可在登录后同步。已填写的模型参与精确寻址与 auto 路由。</p>
    <label>服务 QPS 限制<input id="f-qps" type="number" min="0" step="1" value="0" required></label><p class="hint">0 表示不限制。所有模型、账号和生成重试共用每个滚动一秒窗口的额度；Auto 和指定模型请求均生效。</p>
    <details class="drawer-section"><summary>高级配置</summary><div>
      <div class="form-grid"><label>优先级<input id="f-priority" type="number" min="0" max="255" value="${b?.priority ?? 100}" required></label><label>费用类型<select id="f-cost">${Object.entries(costs).map(([k, v]) => `<option value="${k}">${v}</option>`).join('')}</select></label></div>
      <label>每日 Token 预算<input id="f-budget" type="number" min="0" value="${b?.daily_token_budget ?? 0}" required></label><p class="hint">0 表示不限制。Auto 使用服务优先级、费用层级和日预算。</p>
      <label>后端选项 JSON<textarea id="f-options" rows="3">${esc(b?.options || '')}</textarea></label><label>能力覆盖 JSON<textarea id="f-capabilities" rows="2">${esc(b?.capabilities || '')}</textarea></label><label>状态<select id="f-enabled"><option value="true">启用</option><option value="false">停用</option></select></label>
    </div></details><div class="actions"><button class="button" type="button" data-action="close-drawer">取消</button><button class="button primary" type="submit" id="save-provider">预览配置变更</button></div></form>`);
  $('f-kind').value = b?.kind || 'openai_compat';
  $('f-cost').value = b?.cost_tier || 'unknown';
  $('f-enabled').value = String(b?.enabled ?? true);
  $('backend-form').expectedRevision = state.configuration?.desired_revision;
  const spec = state.configuration?.document.services.find((service) => service.id === id);
  $('f-qps').value = spec?.policy.qps ?? jsonObject(b?.options || '', '后端选项').routing?.qps ?? 0;
  $('backend-form').serviceSpec = spec ? JSON.parse(JSON.stringify(spec)) : null;
  if (spec) {
    $('f-options').value = JSON.stringify(spec.config.options, null, 2);
    $('f-capabilities').value = JSON.stringify(spec.config.capabilities, null, 2);
  }
  renderPluginFields();
  updateProviderForm();
  $('backend-form').addEventListener('submit', (e) => { e.preventDefault(); busy($('save-provider'), saveProvider); });
  $('f-url').addEventListener('input', updateProviderForm);
  $('f-enabled').addEventListener('change', updateProviderForm);
  $('f-preset')?.addEventListener('change', (e) => applyPreset(e.target.value));
  $('f-options').addEventListener('change', () => { try { renderPluginFields(); } catch (error) { toast(error.message, true); } });
  trackDraft($('backend-form'));
}
/** 根据鉴权类型调整字段，不在页面要求私有账号填写 API Key。 */
function updateProviderForm() {
  const privateKind = isPrivate({ kind: $('f-kind').value });
  $('provider-kind-hint').textContent = privateKind ? '逆向 API · 保存服务后登录账号' : '模型网关 · 使用 OpenAI 兼容接口与 API Key 接入';
  $('private-login-hint').textContent = $('f-kind').value === 'raccoon' ? '在新标签页完成小浣熊官方授权，账号自动保存并同步模型。浏览器与代理需在同一台电脑。支持每日登录保留积分，以当天积分批次的实际到期时间确认结果。' : $('f-kind').value === 'monkeycode' ? '在代理电脑的独立 Chrome 窗口中完成官方登录，账号自动保存并同步模型。支持积分查询、手动和每日自动签到。登录页保留地址栏和标签栏。' : $('f-kind').value === 'minimax_code' ? '在 MiniMax 官方网页完成独立 OAuth 授权，账号自动保存。支持服务器部署、模型同步、积分查询和签到。' : $('f-kind').value === 'autoglm' ? '国内站使用手机号验证，国际站使用 Z.ai / Google 授权，登录后账号自动保存。支持积分查询、目录同步和每日签到。' : '保存服务后，通过浏览器登录添加账号。';
  if ($('f-kind').value === 'trae_personal') $('private-login-hint').textContent = '使用 TRAE CN 个人账号在官方网页授权，账号自动保存并同步模型。浏览器与代理需运行在同一台电脑。支持手动和每日自动签到、官方积分余额查询。';
  $('private-fields').hidden = !privateKind;
  $('provider-gateway-fields').hidden = privateKind;
  $('f-url').required = !privateKind;
  const modelscope = isModelScope({ kind: $('f-kind').value, base_url: $('f-url').value });
  $('modelscope-guidance').hidden = !modelscope;
  const existing = state.backends.find((b) => b.id === $('f-id').value);
  $('f-key').required = modelscope && !existing?.has_api_key && $('f-enabled').value === 'true';
  $('f-key').placeholder = existing?.has_api_key ? '留空保留已保存的密钥' : modelscope ? '填写 ModelScope Access Token' : '上游要求鉴权时填写';
  window.SelectControls?.refresh();
}
/** 解析本地 schema 引用，表单不从外部地址加载代码或 schema。 */
function localSchema(schema, root) {
  if (schema?.$ref?.startsWith('#/')) return schema.$ref.slice(2).split('/').reduce((value, key) => value?.[key], root) || schema;
  return schema || {};
}
/** 渲染插件 options 中的常用标量，复杂字段保留在可编辑 JSON 中。 */
function renderPluginFields() {
  const plugin = providerPlugin({ kind: $('f-kind').value });
  const root = plugin?.config_schema || {};
  const schema = localSchema(root.properties?.options, root);
  const options = jsonObject($('f-options').value, '后端选项');
  const requiredComplex = (schema.required || []).filter((key) => {
    const field = localSchema(schema.properties?.[key], root);
    return !field.enum && !['string', 'boolean', 'number', 'integer'].includes(field.type);
  });
  $('plugin-fields').innerHTML = Object.entries(schema.properties || {}).map(([key, field]) => {
    if (['routing', 'headers'].includes(key)) return '';
    field = localSchema(field, root);
    const type = Array.isArray(field.type) ? field.type.find((item) => item !== 'null') : field.type;
    if (!field.enum && !['string', 'boolean', 'number', 'integer'].includes(type)) return '';
    const value = options[key];
    const required = schema.required?.includes(key);
    const label = esc(field.title || key);
    const attribute = `data-plugin-option="${esc(key)}" data-type="${esc(type || 'string')}" ${required ? 'required' : ''}`;
    if (field.enum || type === 'boolean') {
      const choices = field.enum || [true, false];
      return `<label>${label}<select ${attribute}><option value="">插件默认值</option>${choices.map((choice) => `<option value="${esc(JSON.stringify(choice))}" ${choice === value ? 'selected' : ''}>${esc(String(choice))}</option>`).join('')}</select></label>`;
    }
    return `<label>${label}<input ${attribute} type="${type === 'string' ? 'text' : 'number'}" ${type === 'integer' ? 'step="1"' : 'step="any"'}
      ${field.minimum == null ? '' : `min="${esc(field.minimum)}"`} ${field.maximum == null ? '' : `max="${esc(field.maximum)}"`}
      value="${esc(value ?? '')}" placeholder="${esc(field.default ?? '插件默认值')}"></label>`;
  }).join('');
  if (requiredComplex.length) {
    $('plugin-fields').innerHTML += `<p class="hint">必填高级字段：${esc(requiredComplex.join(', '))}，请在下方后端选项 JSON 填写。</p>`;
    $('f-options').closest?.('details')?.setAttribute('open', '');
  }
  renderConfigHelpers(plugin, options);
  window.SelectControls?.refresh();
}
/** 按插件声明渲染复杂配置的采集与导入入口，地址只允许同源相对路径。 */
function renderConfigHelpers(plugin, options) {
  const container = $('plugin-config-helpers');
  if (!container) return;
  container.innerHTML = (plugin?.ui?.config_helpers || []).map((helper, index) => {
    if (![helper.capture_path, helper.validate_path, helper.script_path].every((path) => typeof path === 'string' && /^\/api\/[a-zA-Z0-9_./-]+$/.test(path))) return '';
    const value = options[helper.option];
    return `<section class="drawer-section config-helper" id="config-helper-${index}">
      <h3>${esc(helper.title)}</h3><p class="hint">${esc(helper.instructions)}</p>
      <div class="button-row"><button class="button small" type="button" data-action="config-capture" data-id="${index}">${icon('refresh')}自动获取本地</button>
      <a class="button small" href="${esc(helper.script_path)}" download="${esc(helper.script_filename)}">${icon('download')}下载采集脚本</a>
      <button class="button small" type="button" data-action="config-import-file" data-id="${index}">${icon('upload')}导入 JSON 文件</button></div>
      <input id="config-file-${index}" type="file" accept=".json,application/json" hidden>
      <details><summary>粘贴 JSON 手动导入</summary><label>设备信息 JSON<textarea id="config-json-${index}" rows="4" placeholder="粘贴采集报告或设备对象"></textarea></label>
      <button class="button small" type="button" data-action="config-import-json" data-id="${index}">校验并导入</button></details>
      <div id="config-candidates-${index}" hidden><label>选择客户端<select id="config-choice-${index}"></select></label>
      <button class="button small" type="button" data-action="config-use-candidate" data-id="${index}">使用所选设备</button></div>
      <p id="config-status-${index}" class="hint" role="status">${value ? '已填写设备信息；保存前可重新采集或导入。' : '尚未填写设备信息。'}</p>
      ${value ? `<dl class="detail-summary">${(helper.fields || []).map((field) => `<div><dt>${esc(field.title)}</dt><dd>${esc(value[field.key] || '未提供')}</dd></div>`).join('')}</dl>` : ''}
      <p class="hint">采集与导入仅填写当前表单；点击「预览配置变更」后应用才保存。</p>
    </section>`;
  }).join('');
  (plugin?.ui?.config_helpers || []).forEach((_, index) => {
    $(`config-file-${index}`)?.addEventListener('change', (event) => {
      const file = event.target.files?.[0];
      event.target.value = '';
      if (file) busy(null, () => importConfigHelper(index, file));
    });
  });
}
/** 捕获表单、组件和请求序号，迟到响应不能覆盖切换后的服务或后发导入。 */
function configHelperContext(index) {
  const form = $('backend-form');
  const plugin = providerPlugin({ kind: $('f-kind')?.value });
  const helper = plugin?.ui?.config_helpers?.[index];
  const node = $(`config-helper-${index}`);
  if (!form || !helper || !node) throw new Error('请先打开支持采集的服务配置');
  node.requestSequence = (node.requestSequence || 0) + 1;
  return { form, plugin, helper, node, index, sequence: node.requestSequence };
}
/** 判断异步结果仍属于可见的同一表单和同一采集请求。 */
function configHelperCurrent(context) {
  return $('drawer')?.open && $('backend-form') === context.form && $(`config-helper-${context.index}`) === context.node
    && $('f-kind')?.value === context.plugin.id && context.node.requestSequence === context.sequence;
}
/** 将服务端已校验的候选暂存到当前组件，多客户端由用户显式选择。 */
function showConfigHelperReport(context, report) {
  if (!configHelperCurrent(context)) return;
  if (!report.candidates?.length) throw new Error((report.warnings || []).join('\n') || '没有可导入的完整设备信息');
  context.node.report = report;
  if (report.candidates.length === 1) { applyConfigHelper(context, report.candidates[0]); return; }
  $(`config-choice-${context.index}`).innerHTML = report.candidates.map((candidate, index) => `<option value="${index}">${esc(candidate.label)}</option>`).join('');
  $(`config-candidates-${context.index}`).hidden = false;
  $(`config-status-${context.index}`).textContent = `找到 ${report.candidates.length} 个客户端，请选择后使用。` + (report.warnings?.length ? '\n' + report.warnings.join('\n') : '');
  window.SelectControls?.refresh();
}
/** 发起插件声明的本机采集，采集范围由插件说明呈现。 */
async function captureConfigHelper(index) {
  const context = configHelperContext(index);
  $(`config-status-${index}`).textContent = '正在读取 FMP 主机的客户端设备信息…';
  try {
    const report = await api(context.helper.capture_path, { method: 'POST', body: {} });
    showConfigHelperReport(context, report);
  } catch (error) {
    if (!configHelperCurrent(context)) return;
    $(`config-status-${index}`).textContent = error.message;
    throw error;
  }
}
/** 文件和粘贴内容共用服务端校验，不在浏览器复制插件业务规则。 */
async function importConfigHelper(index, file = null) {
  const context = configHelperContext(index);
  try {
    if (file && file.size > 256 * 1024) throw new Error('设备 JSON 文件不能超过 256 KiB');
    const raw = file ? await file.text() : $(`config-json-${index}`).value;
    let input;
    try { input = JSON.parse(raw); } catch { throw new Error('设备信息必须是合法 JSON'); }
    if (!configHelperCurrent(context)) return;
    const report = await api(context.helper.validate_path, { method: 'POST', body: input });
    showConfigHelperReport(context, report);
  } catch (error) {
    if (!configHelperCurrent(context)) return;
    $(`config-status-${index}`).textContent = error.message;
    throw error;
  }
}
/** 合并当前标量编辑及设备对象，保留其他选项、凭证引用与原有保存版本。 */
function applyConfigHelper(context, candidate) {
  if (!configHelperCurrent(context) || !candidate) return;
  const options = pluginFormOptions();
  options[context.helper.option] = candidate.value;
  $('f-options').value = JSON.stringify(options, null, 2);
  renderPluginFields();
  toast('设备信息已填入表单，请预览并应用配置变更');
}
/** 只应用用户所选的已校验候选，不按数组顺序推断设备。 */
function useConfigHelperCandidate(index) {
  const context = configHelperContext(index);
  const candidate = context.node.report?.candidates?.[Number($(`config-choice-${index}`).value)];
  applyConfigHelper(context, candidate);
}
/** 将标量输入与高级 JSON 合并；空值删除显式覆盖以恢复插件默认值。 */
function pluginFormOptions() {
  const options = jsonObject($('f-options').value, '后端选项');
  $('plugin-fields').querySelectorAll('[data-plugin-option]').forEach((field) => {
    const key = field.dataset.pluginOption;
    if (field.value === '') { delete options[key]; return; }
    options[key] = field.tagName === 'SELECT' ? JSON.parse(field.value) : field.dataset.type === 'string' ? field.value : Number(field.value);
  });
  return options;
}
/** 切换预设时完整替换服务配置，避免将上一服务的密钥或选项带入新服务。 */
function applyPreset(id) {
  const plugin = id.startsWith('plugin:') ? providerPlugin({ kind: id.slice(7) }) : null;
  const preset = plugin ? { name: plugin.display_name, plugin_id: plugin.id, config: { base_url: '', models: [], options: {} }, policy: {} } : id ? state.presets.find((p) => p.id === id) : {
    name: '', kind: 'openai_compat', base_url: '', models_hint: '', priority: 100, cost_tier: 'unknown',
  };
  if (!preset) return;
  $('f-preset').value = id;
  $('f-name').value = preset.name.replace(/（.*?）/g, '');
  $('f-kind').value = preset.plugin_id || preset.kind;
  $('f-url').value = preset.config?.base_url ?? preset.base_url;
  $('f-models').value = preset.config?.models?.join(',') ?? preset.models_hint;
  $('f-priority').value = preset.policy?.priority ?? preset.priority ?? 100;
  $('f-cost').value = preset.policy?.cost_tier ?? preset.cost_tier ?? 'unknown';
  $('f-key').value = '';
  if ($('backend-form')) $('backend-form').secretRef = null;
  $('f-options').value = preset.config?.options ? JSON.stringify(preset.config.options, null, 2) : '';
  $('f-capabilities').value = preset.config?.capabilities ? JSON.stringify(preset.config.capabilities, null, 2) : '';
  $('f-budget').value = preset.policy?.daily_token_budget ?? 0;
  $('f-qps').value = preset.policy?.qps ?? 0;
  $('f-enabled').value = 'false';
  renderPluginFields();
  updateProviderForm();
}
/** 保存服务并转入详情，继续完成登录和目录同步。 */
async function saveProvider() {
  const id = $('f-id').value;
  const b = state.backends.find((item) => item.id === id);
  if (isModelScope({ kind: $('f-kind').value, base_url: $('f-url').value }) && $('f-enabled').value === 'true'
    && !$('f-key').value.trim() && !b?.has_api_key && !$('backend-form')?.secretRef) {
    throw new Error('启用 ModelScope 前，请填写官方 Access Token，或先保存为停用服务。');
  }
  const plugin = providerPlugin({ kind: $('f-kind').value });
  if (!plugin) throw new Error('插件未安装，请刷新目录');
  const existing = $('backend-form').serviceSpec;
  const credentials = JSON.parse(JSON.stringify(existing?.credentials || {}));
  const raw = $('f-key').value.trim();
  if (raw) {
    const written = await api('/api/v2/secrets', { method: 'POST', body: { value: raw } });
    credentials.api_key = { secret_ref: written.secret_ref };
    $('backend-form').secretRef = credentials.api_key;
    $('f-key').value = '';
  } else if ($('backend-form').secretRef) credentials.api_key = $('backend-form').secretRef;
  const options = pluginFormOptions();
  if (options.headers && Object.keys(options.headers).length) throw new Error('请求头凭证须通过 credentials.headers 的秘密引用配置；可使用 CLI 写入。');
  const routing = options.routing;
  delete options.routing;
  const service = {
    api_version: 'fmp/v2', kind: 'Service', id: id || managementId(), name: $('f-name').value.trim(),
    plugin_id: plugin.id, plugin_config_version: plugin.config_schema_version,
    enabled: $('f-enabled').value === 'true',
    config: { base_url: $('f-url').value.trim(), models: $('f-models').value.split(',').map((item) => item.trim()).filter(Boolean), options,
      capabilities: jsonObject($('f-capabilities').value, '能力覆盖') },
    credentials, policy: { ...existing?.policy, ...routing, priority: Number($('f-priority').value),
      daily_token_budget: Number($('f-budget').value), cost_tier: $('f-cost').value, qps: Number($('f-qps').value) },
  };
  await previewChange({ services: [service] }, () => openProvider(service.id), $('backend-form').expectedRevision);
}
/** 校验并保存绑定当前版本的计划，应用前提供路径和重建影响。 */
async function previewChange(change, afterApply, revision = state.configuration.desired_revision) {
  const validation = await api('/api/v2/config/validate', { method: 'POST', body: change });
  if (!validation.valid) throw new Error(validation.diagnostics.map((item) => `${item.path}: ${item.message}`).join('\n'));
  const plan = await api('/api/v2/config/plans', { method: 'POST', body: { expected_revision: revision, change } });
  state.plan = { plan, afterApply, key: managementId() };
  modal('预览配置变更', `<p>配置版本 ${plan.expected_revision} · ${plan.changes.length} 项变更</p>
    <div class="notice">${plan.requires_restart ? '需要重启服务' : '应用后生效；在途请求保留其运行实例'}</div>
    <pre>${esc(plan.changes.map((item) => `${item.action} ${item.path}${item.rebuild_connection ? ' · 重建连接' : ''}`).join('\n') || '配置没有变化')}</pre>
    <p class="hint">计划有效至 ${new Date(plan.expires_at * 1000).toLocaleTimeString()}；版本冲突后需要重新预览。</p>
    <div class="actions"><button class="button" data-action="close-modal">返回修改</button><button class="button primary" data-action="apply-plan">应用变更</button></div>`);
}
/** 应用只提交已展示的计划 ID；失败重试使用同一幂等键。 */
async function applyConfigPlan() {
  const pending = state.plan;
  if (!pending) return;
  const result = await api(`/api/v2/config/plans/${encodeURIComponent(pending.plan.id)}/apply`, { method: 'POST', body: { idempotency_key: pending.key } });
  state.plan = null;
  const form = $('backend-form');
  if (form) trackDraft(form);
  $('modal').close();
  await refresh();
  pending.afterApply?.();
  toast(`配置已应用 · 生效版本 ${result.active_revision}`);
}
/** 保留服务所有配置字段和原密钥，只更新指定字段。 */
async function patchProvider(id, changes) {
  const b = state.backends.find((item) => item.id === id);
  if (isModelScope(b) && !b.has_api_key && changes.enabled === true) {
    editProvider(id);
    $('f-enabled').value = 'true';
    updateProviderForm();
    toast('填写 ModelScope Access Token 后保存即可启用');
    return;
  }
  const service = state.configuration.document.services.find((item) => item.id === id);
  await previewChange({ services: [{ ...service, ...changes }] }, () => openProvider(id, false));
}
/** 同步服务模型目录并重新渲染模型选择。 */
async function syncModels(id) {
  await runPluginOperation(id, 'models.sync');
}
/** 编辑或导入账号；已有凭证留空表示保留。 */
function editAccount(id, backendId) {
  cancelLogin();
  const a = state.accounts.find((item) => item.id === id);
  const b = state.backends.find((item) => item.id === (a?.backend_id || backendId));
  modal(a ? '编辑账号' : '手工导入账号', `<p class="muted">所属服务：${esc(b.name)}</p>${b.kind === 'monkeycode' ? '<p class="hint">官网登录会自动保存模型密钥和登录态。需要更新登录态时，请使用「重新登录」。</p>' : b.kind === 'autoglm' ? '<p class="hint">支持 accessToken、refreshToken 或 snake_case 凭证。续期需要登录时的 device_id，JWT 携带时自动提取；region 可指定 cn / global。</p>' : ''}<form id="account-form"><label style="margin-top:16px">账号名称<input id="a-name" required value="${esc(a?.name || '')}"></label><label>凭证 JSON<textarea id="a-secret" rows="5" ${a ? '' : 'required'} placeholder="${a ? '留空保留已保存的凭证' : '粘贴该服务的账号凭证 JSON'}"></textarea></label><label>账号状态<select id="a-enabled"><option value="true">启用</option><option value="false">停用</option></select></label><div class="actions"><button type="button" class="button" data-action="close-modal">取消</button><button id="account-save" class="button primary" type="submit">保存账号</button></div></form>`);
  $('a-enabled').value = String(a?.enabled ?? true);
  window.SelectControls?.refresh();
  trackDraft($('account-form'));
  $('account-form').addEventListener('submit', (e) => { e.preventDefault(); busy($('account-save'), async () => {
    const raw = $('a-secret').value.trim();
    if (raw) jsonObject(raw, '凭证');
    await api('/api/accounts', { method: 'POST', body: { id: a?.id || '', backend_id: b.id, name: $('a-name').value.trim(), credential: raw, enabled: $('a-enabled').value === 'true' } });
    $('modal').close();
    await refresh();
    openProvider(b.id, false);
    toast('账号已保存');
  }); });
}
/** 在清楚说明影响后执行删除操作。 */
function confirmDelete(type, id) {
  const item = (type === 'provider' ? state.backends : state.accounts).find((r) => r.id === id);
  if (type === 'provider' && accountsOf(id).length) {
    toast('此服务仍有关联账号，请先在服务详情中移除账号，再删除服务。', true);
    return;
  }
  const impact = type === 'provider' ? '删除此服务的本地配置，该服务的模型调用名称将无法再使用。' : '只移除本代理保存的账号配置，不注销上游账号。';
  modal(`删除${type === 'provider' ? '服务' : '账号'}`, `<p>确认删除「${esc(item.name)}」？</p><p class="hint">${impact}</p><div class="actions"><button class="button" data-action="close-modal">取消</button><button id="confirm-delete" class="button danger">确认删除</button></div>`);
  $('confirm-delete').addEventListener('click', () => busy($('confirm-delete'), async () => {
    await previewChange(type === 'provider' ? { remove_services: [id] } : { remove_accounts: [id] }, () => {
      if (type === 'provider') { $('drawer').close(); state.drawerProvider = null; }
      else openProvider(item.backend_id, false);
    });
  }));
}
/** 终止当前页面的登录轮询，阻止旧响应覆盖新会话。 */
function cancelLogin() {
  if (state.login) {
    const session = state.login;
    clearTimeout(session.timer);
    session.controller.abort();
    state.login = null;
    if (session.operationId) {
      api(`/api/v2/operations/${encodeURIComponent(session.operationId)}`).then(cancelDetachedOperation).catch((error) => toast(error.message, true));
      return;
    }
    if (['autoglm', 'minimax_code', 'monkeycode', 'raccoon', 'trae_personal'].includes(session.kind) && session.sessionId) cancelOAuth(session);
  }
}
/** 将取消传到本机回调服务；过期或已完成的会话不影响页面关闭。 */
function cancelOAuth(session) {
  api(`/api/oauth/${session.kind}/cancel`, { method: 'POST', body: { backend_id: session.id, session_id: session.sessionId } }).catch(() => {});
}
/** 按官方前置配置完成安全验证，只有上游接受回执才继续授权。 */
async function getAutoGlmAuthorization(session, provider) {
  const prepared = await api('/api/oauth/autoglm/captcha-config', { method: 'POST', body: { backend_id: session.id }, signal: session.controller.signal });
  if (state.login !== session) throw new Error('授权已取消');
  const config = prepared.config;
  /** 沿用设备 UUID，关闭期间迟到的响应仍会取消服务端会话。 */
  const start = async (proof) => {
    const result = await api('/api/v2/operations', { method: 'POST', timeout: 30000, body: {
      service_id: session.id, action: 'auth.start', idempotency_key: session.key,
      input: { provider, device_id: prepared.device_id, ...(proof ? { ali_captcha_verify_param: proof } : {}) },
    } });
    session.operationId = result.operation_id;
    if (state.login !== session) {
      cancelDetachedOperation(result);
      throw new Error('授权已取消');
    }
    return result;
  };
  if (config?.enabled === false) return start();
  if (config?.enabled !== true || config.captcha_supplier && config.captcha_supplier !== 'aliyun'
    || !['cn', 'sgp', 'ga'].includes(config.region) || !config.prefix || !config.scene_id) {
    throw new Error('上游安全验证方式已变化，请更新代理后重试');
  }
  $('modal-body').innerHTML = '<p class="muted">完成官方安全验证后，继续前往授权页面。</p><iframe id="glm-captcha-frame" title="官方安全验证" style="width:100%;height:360px;border:0"></iframe><p class="hint" id="glm-captcha-status">正在加载安全验证组件…</p><div class="actions"><button class="button" data-action="close-modal">取消</button></div>';
  const frame = $('glm-captcha-frame');
  const frameUrl = new URL('/assets/autoglm-captcha.html', location.origin);
  frameUrl.search = new URLSearchParams({ region: config.region, prefix: config.prefix, scene_id: config.scene_id });
  return new Promise((resolve, reject) => {
    let settled = false;
    let authorizedResult = null;
    let submitting = false;
    /** 结束当前等待并释放计时器和监听，不影响其他窗口。 */
    const finish = (error, result) => {
      if (settled) return;
      settled = true;
      clearTimeout(session.timer);
      session.controller.signal.removeEventListener('abort', aborted);
      window.removeEventListener('message', receive);
      frame.src = 'about:blank';
      if (error) reject(error); else resolve(result);
    };
    /** 关闭控制台授权窗口后停止创建回调会话。 */
    const aborted = () => finish(new Error('授权已取消'));
    session.controller.signal.addEventListener('abort', aborted, { once: true });
    session.timer = setTimeout(() => finish(new Error('安全验证已超时，请重新开始')), 600000);
    /** 只接收当前同源验证页面，完成事件不能代替上游接受回执。 */
    async function receive(event) {
      if (settled || state.login !== session || event.origin !== location.origin || event.source !== frame.contentWindow) return;
      const data = event.data;
      if (data?.type === 'fmp-captcha-ready') $('glm-captcha-status').textContent = '按上方官方提示完成安全验证。';
      if (data?.type === 'fmp-captcha-error') { finish(new Error(data.message || '安全验证加载失败')); return; }
      if (data?.type === 'fmp-captcha-done' && authorizedResult) { finish(null, authorizedResult); return; }
      if (data?.type !== 'fmp-captcha-proof' || submitting || authorizedResult) return;
      submitting = true;
      try {
        if (typeof data.proof !== 'string' || !data.proof.trim()) throw new Error('安全验证未返回有效回执');
        $('glm-captcha-status').textContent = '正在校验并获取授权页面…';
        authorizedResult = await start(data.proof);
        if (!settled) frame.contentWindow.postMessage({ type: 'fmp-captcha-result', id: data.id, accepted: true }, location.origin);
      } catch (error) {
        if (!settled) frame.contentWindow.postMessage({ type: 'fmp-captcha-result', id: data.id, accepted: false, message: error.message }, location.origin);
        if (state.login === session) $('glm-captcha-status').textContent = error.message;
      } finally { submitting = false; }
    }
    window.addEventListener('message', receive);
    frame.src = frameUrl.href;
  });
}
/** 开始独立上游网页登录。 */
async function login(id) {
  cancelLogin();
  const b = state.backends.find((item) => item.id === id);
  if (b.kind === 'autoglm') return autoglmLogin(b);
  await startBrowserLogin(id);
}
/** 在点击时打开授权窗口，每个流程持有独立服务端会话。 */
async function startBrowserLogin(id, provider) {
  cancelLogin();
  const b = state.backends.find((item) => item.id === id);
  const session = { id, kind: b.kind, controller: new AbortController(), timer: null, key: managementId() };
  state.login = session;
  modal(`登录 ${b.name}`, '<div class="oauth-stage"><span class="spinner"></span><div><strong>正在获取授权地址</strong><p>使用你的上游账号完成登录。</p></div></div>');
  try {
    const operation = b.kind === 'autoglm' && !autoGlmDomestic(b) ? await getAutoGlmAuthorization(session, provider)
      : await api('/api/v2/operations', { method: 'POST', body: { service_id: id, action: 'auth.start', input: {}, idempotency_key: session.key } });
    session.operationId = operation.operation_id;
    if (state.login !== session) { cancelDetachedOperation(operation); return; }
    watchOperation(session, operation);
  } catch (error) {
    if (state.login === session) { cancelLogin(); $('modal-body').innerHTML = `<div class="notice error">${esc(error.message)}</div><button class="button" data-action="login" data-id="${esc(id)}">重新开始</button>`; }
  }
}
/** 从动作目录检查可执行性，再显式提交远程操作。 */
async function runPluginOperation(id, action, input = {}) {
  const b = state.backends.find((item) => item.id === id);
  if (!providerPlugin(b)?.operations.some((item) => item.id === action)) throw new Error('插件未实现该操作');
  const operation = await api('/api/v2/operations', { method: 'POST', body: { service_id: id, action, input, idempotency_key: managementId() } });
  const session = { id, operationId: operation.operation_id, timer: null };
  modal(`${b.name} · ${action}`, '');
  watchOperation(session, operation);
  return operation;
}
/** 专属动作的必填输入通过 JSON 编辑器提交，服务端仍执行完整 schema 校验。 */
async function requestPluginOperation(id, action) {
  const descriptor = providerPlugin(state.backends.find((item) => item.id === id))?.operations.find((item) => item.id === action);
  if (!descriptor) throw new Error('插件未实现该操作');
  if (!descriptor.input_schema.required?.length) return runPluginOperation(id, action);
  modal(descriptor.title, `<form id="operation-form"><label>动作参数 JSON<textarea id="operation-input" rows="6" required>{}</textarea></label>
    <p class="hint">必填字段：${esc(descriptor.input_schema.required.join(', '))}</p><details><summary>字段 schema</summary><pre>${esc(JSON.stringify(descriptor.input_schema, null, 2))}</pre></details>
    <button class="button primary" id="operation-run" type="submit">执行操作</button></form>`);
  $('operation-form').addEventListener('submit', (event) => {
    event.preventDefault();
    busy($('operation-run'), () => runPluginOperation(id, action, jsonObject($('operation-input').value, '动作参数')));
  });
}
/** 只接受服务端返回的安全 URL；插件负责官方授权地址的协议校验。 */
function operationUrl(value) {
  if (!value) return null;
  const url = new URL(value, location.origin);
  if (url.username || url.password || !(url.protocol === 'https:' || url.origin === location.origin && url.pathname.startsWith('/auth/'))) return null;
  return url.href;
}
/** 展示终态、等待用户和未知结果，查询失败时允许继续读取同一操作。 */
function operationMarkup(operation) {
  const statuses = { pending: '排队中', running: '执行中', waiting_user: '等待授权', succeeded: '已完成', failed: '执行失败', cancelled: '已取消', expired: '已过期', unknown: '结果未知' };
  const url = operationUrl(operation.next_action?.url);
  return `<div class="notice">${esc(statuses[operation.status] || operation.status)}</div>
    ${operation.status === 'unknown' ? '<p class="hint">不能确认上游是否执行，请核对账号状态；不会自动重新执行。</p>' : ''}
    ${operation.next_action ? `<p>${esc(operation.next_action.message || '请完成官方登录，再回到这里查看结果。')}</p>
      ${operation.next_action.user_code ? `<pre>${esc(operation.next_action.user_code)}</pre>` : ''}` : ''}
    ${operation.error ? `<p class="notice error">${esc(operation.error.message)} · ${esc(operation.error.code)}</p>` : ''}
    ${operation.result ? `<details><summary>查看结果</summary><pre>${esc(JSON.stringify(operation.result, null, 2))}</pre></details>` : ''}
    ${operation.action === 'auth.start' && operation.status === 'succeeded' && operation.result?.models_synced !== true
      ? `<p class="hint">账号已保存。${esc(operation.result?.sync_error || '可以继续同步模型目录。')}</p><button class="button" data-action="sync-models" data-id="${esc(operation.service_id)}">同步模型</button>` : ''}
    <p class="hint mono">${esc(operation.operation_id)}</p><div class="actions">
      ${url ? `<a class="button primary" target="_blank" rel="noopener noreferrer" href="${esc(url)}">打开授权页 ↗</a>` : ''}
      ${operation.status === 'waiting_user' ? `<button class="button" data-action="cancel-operation" data-id="${esc(operation.operation_id)}">取消授权</button>` : ''}
      <button class="button" data-action="close-modal">关闭</button></div>`;
}
/** 单次查询结束才安排后续轮询；页面关闭后操作历史仍可恢复。 */
function watchOperation(session, operation) {
  state.operations = [operation, ...state.operations.filter((item) => item.operation_id !== operation.operation_id)];
  session.operationId = operation.operation_id;
  if ($('modal').open && (!state.login || state.login === session)) {
    $('modal-body').dataset.operationId = operation.operation_id;
    $('modal-body').innerHTML = operationMarkup(operation);
  }
  if (!['pending', 'running', 'waiting_user'].includes(operation.status)) {
    if (state.login === session) state.login = null;
    refresh();
    return;
  }
  session.timer = setTimeout(async () => {
    try {
      const latest = await api(`/api/v2/operations/${encodeURIComponent(operation.operation_id)}`);
      if (operation.action === 'auth.start' && state.login !== session) { cancelDetachedOperation(latest); return; }
      if ($('modal-body').dataset.operationId !== operation.operation_id) return;
      watchOperation(session, latest);
    } catch (error) {
      if ($('modal-body').dataset.operationId === operation.operation_id) $('modal-body').innerHTML = `<p class="notice error">${esc(error.message)}</p><button class="button" data-action="operation" data-id="${esc(operation.operation_id)}">继续查询</button>`;
    }
  }, 1500);
}
/** 开始响应迟到时等到会话可取消再释放，最多等待服务器操作期限。 */
function cancelDetachedOperation(operation) {
  if (operation.status === 'waiting_user') {
    api(`/api/v2/operations/${encodeURIComponent(operation.operation_id)}/cancel`, { method: 'POST', body: {} }).catch((error) => toast(error.message, true));
  } else if (['pending', 'running'].includes(operation.status) && Date.now() < operation.deadline * 1000) {
    setTimeout(async () => {
      try { cancelDetachedOperation(await api(`/api/v2/operations/${encodeURIComponent(operation.operation_id)}`)); }
      catch (error) { toast(`取消未完成，请在任务历史中核对：${error.message}`, true); }
    }, 1500);
  }
}
/** 恢复已有操作的状态而不是重新执行远程动作。 */
async function openOperation(id) {
  cancelLogin();
  const operation = await api(`/api/v2/operations/${encodeURIComponent(id)}`);
  const session = { id: operation.service_id, operationId: id, timer: null, controller: new AbortController() };
  if (operation.action === 'auth.start' && ['pending', 'running', 'waiting_user'].includes(operation.status)) state.login = session;
  modal('操作详情', '');
  watchOperation(session, operation);
}
/** 未显式选择国际站时使用国内登录，与后端默认区域一致。 */
function autoGlmDomestic(b) {
  try { return JSON.parse(b.options || '{}').region !== 'global'; }
  catch { throw new Error('服务选项格式不正确，请重新保存服务'); }
}
/** 国内接受同源会话页面，海外精确匹配所选提供商的官方 HTTPS 域。 */
function browserLoginUrl(result, b, provider) {
  const url = new URL(result.login_url, location.origin);
  if (b.kind === 'autoglm' && autoGlmDomestic(b)) {
    if (result.kind !== 'sms_browser' || !/^[0-9a-f-]{36}$/.test(result.session_id || '')
      || url.origin !== location.origin || url.pathname !== `/auth/autoglm/${result.session_id}`
      || url.username || url.password || url.hash || url.search) throw new Error('国内登录地址不属于当前代理会话');
  } else {
    const hosts = b.kind === 'minimax_code'
      ? [JSON.parse(b.options || '{}').region === 'global' ? 'account.minimax.io' : 'account.minimax.cn']
      : b.kind === 'raccoon' ? ['xiaohuanxiong.com']
      : b.kind === 'trae_personal' ? ['www.trae.cn']
      : b.kind === 'autoglm'
      ? [provider === 'google' ? 'accounts.google.com' : 'chat.z.ai']
      : ['qoder.com', 'qoder.com.cn', 'www.workbuddy.cn', 'www.workbuddy.ai', 'www.codebuddy.cn', 'www.codebuddy.ai', 'copilot.tencent.com'];
    if (url.protocol !== 'https:' || url.username || url.password || url.port || !hosts.includes(url.hostname)) throw new Error('授权地址不属于已知官方登录站点');
    if (b.kind === 'raccoon') {
      const params = url.searchParams;
      const redirect = params.get('redirect') || '';
      const nonce = params.get('state') || '';
      if (result.kind !== 'loopback_oauth' || url.pathname !== '/authorize' || url.hash
        || params.get('login_source') !== 'desktop' || params.get('desktop_callback') !== 'loopback'
        || !/^[a-f0-9]{64}$/.test(nonce)
        || !/^http:\/\/127\.0\.0\.1:[1-9][0-9]{3,4}\/auth\/callback$/.test(redirect)
        || Number(new URL(redirect).port) < 1024 || Number(new URL(redirect).port) > 65535
        || [...params.keys()].some((key) => params.getAll(key).length !== 1)) throw new Error('小浣熊授权回调参数无效');
    }
    if (b.kind === 'trae_personal') {
      const params = url.searchParams;
      const callback = params.get('auth_callback_url') || '';
      if (result.kind !== 'loopback_oauth' || url.pathname !== '/authorization' || url.hash
        || params.get('client_id') !== 'ono9krqynydwx5' || params.get('auth_type') !== 'local'
        || params.get('hide_saas_login') !== 'true'
        || !/^[a-f0-9]{64}$/.test(params.get('login_trace_id') || '')
        || !/^http:\/\/127\.0\.0\.1:[1-9][0-9]{3,4}\/authorize$/.test(callback)
        || Number(new URL(callback).port) < 1024 || Number(new URL(callback).port) > 65535
        || [...params.keys()].some((key) => params.getAll(key).length !== 1)) throw new Error('TRAE 个人授权回调参数无效');
    }
  }
  return url;
}
/** 国内使用短信会话，海外选择官方提供商完成授权回调。 */
function autoglmLogin(b) {
  if (autoGlmDomestic(b)) return startBrowserLogin(b.id);
  modal(`登录 ${b.name}`, `<p class="muted">在官方页面登录，完成授权后账号会自动保存。</p><p class="hint">浏览器与代理服务需运行在同一台电脑；授权后回到此处即可。</p><div class="actions"><button class="button primary" data-action="autoglm-browser" data-provider="zai" data-id="${esc(b.id)}">使用 Z.ai 授权 ↗</button><button class="button" data-action="autoglm-browser" data-provider="google" data-id="${esc(b.id)}">使用 Google 授权 ↗</button></div>`);
}
/** 单次轮询结束后再安排下一次，不并发重复领取授权结果。 */
async function pollLogin(session, b) {
  if (state.login !== session) return;
  try {
    if (Date.now() >= session.deadline) throw new Error('授权已超时，请重新开始');
    const result = await api(`/api/oauth/${b.kind}/poll`, { method: 'POST', body: { backend_id: b.id, ...(session.sessionId ? { session_id: session.sessionId } : {}) }, signal: session.controller.signal, timeout: 60000 });
    if (state.login !== session) return;
    if (result.status === 'done') {
      state.login = null;
      await refresh();
      openProvider(b.id, false);
      if (['monkeycode', 'raccoon', 'trae_personal'].includes(b.kind)) {
        $('modal-body').innerHTML = `<div class="oauth-stage"><span class="spinner"></span><div><strong>账号 ${esc(result.name)} 已保存</strong><p>正在同步模型目录…</p></div></div>`;
        try {
          if (result.reload_error) throw new Error(result.reload_error);
          if (result.models_synced === false) throw new Error(result.sync_error || '模型目录同步失败');
          if (result.models_synced !== true) await syncModels(b.id);
          $('modal-body').innerHTML = `<div class="notice">账号 ${esc(result.name)} 已保存，模型目录已同步。</div><div class="actions"><button class="button primary" data-action="close-modal">完成</button></div>`;
        } catch (error) {
          $('modal-body').innerHTML = `<div class="notice">账号 ${esc(result.name)} 已保存。</div><div class="notice error">模型同步失败：${esc(error.message)}</div><div class="actions"><button class="button" data-action="close-modal">关闭</button><button class="button primary" data-action="login-sync" data-id="${esc(b.id)}">重试同步模型</button></div>`;
        }
        return;
      }
      $('modal-body').innerHTML = `<div class="notice">账号 ${esc(result.name || result.uid)} 已保存。</div><p class="hint">接下来同步模型目录，开始使用。</p><div class="actions"><button class="button" data-action="close-modal">完成</button><button class="button primary" data-action="login-sync" data-id="${esc(b.id)}">同步模型</button></div>`;
      toast('账号授权成功');
      return;
    }
    $('login-status').textContent = `${b.kind === 'monkeycode' ? '请在独立登录窗口完成官方验证，完成后自动接入' : '等待授权完成'} · 剩余约 ${Math.max(0, Math.ceil((session.deadline - Date.now()) / 1000))} 秒`;
    session.timer = setTimeout(() => pollLogin(session, b), 2000);
  } catch (error) {
    if (state.login !== session) return;
    cancelLogin();
    $('modal-body').innerHTML = `<div class="notice error">${esc(error.message)}</div><button class="button" data-action="login" data-id="${esc(b.id)}">重新开始</button>`;
  }
}
/** 展示每次上游尝试及其准入负载，旧记录缺少快照时保留原有信息。 */
function recordAttemptDetails(attempt, index) {
  const load = attempt.routing?.admission;
  const admission = load ? `<dl class="detail-summary"><div><dt>准入时服务并发 / 容量</dt><dd>${number(load.service_in_flight)} / ${number(load.service_capacity)}</dd></div><div><dt>准入时账号并发 / 容量</dt><dd>${number(load.account_in_flight)} / ${number(load.account_capacity)}</dd></div><div><dt>服务 QPS 限制</dt><dd>${load.service_qps === 0 ? '不限' : number(load.service_qps)}</dd></div><div><dt>近 1 秒准入数（含预占）</dt><dd>${number(load.service_submissions_in_window)}</dd></div></dl>` : '';
  return `<div class="notice"><strong>${index + 1}. ${esc(attempt.model)} · ${requestOutcome(attempt)[0]}</strong><div class="secondary">开始 +${number(attempt.offset_ms)} ms · 耗时 ${number(attempt.duration_ms)} ms · 上游错误 HTTP ${attempt.upstream_status ?? '—'}</div><code>${esc(attempt.span_id)}</code>${attempt.routing?.account_id ? `<p class="secondary">账号 ${esc(attempt.routing.account_id)}</p>` : ''}${admission}${attempt.error ? `<p>${esc(attempt.error)}</p>` : ''}</div>`;
}
/** 展示入站字节和归一图片规模，拒收的部分读取与旧记录保持明确。 */
function recordRequestScaleDetails(record) {
  const completion = record.request_body_complete === true ? '完整读取' : record.request_body_complete === false ? '未完整读取' : '读取状态未知';
  return `<dl class="detail-summary"><div><dt>请求体已读取 / 上限（字节）</dt><dd>${number(record.request_body_bytes)} / ${number(record.request_body_limit_bytes)} · ${completion}</dd></div><div><dt>客户端声明长度（字节）</dt><dd>${number(record.request_content_length)}</dd></div><div><dt>消息 / 图片</dt><dd>${number(record.message_count)} 条 / ${number(record.image_count)} 张</dd></div><div><dt>图片 URL 总量（字节）</dt><dd>${number(record.image_url_bytes)}</dd></div></dl><p class="hint">字节上限与模型上下文预算分别检查。图片 URL 总量包括 base64，但不计入文本字符数。旧记录缺少的计数显示为未知。</p>`;
}
/** 展示本地无可用目标的准入原因，模型名和原因只按文本渲染。 */
function recordRouteRejections(record) {
  const rejections = record.routing_rejections || [];
  if (!rejections.length) return '';
  return `<p class="hint">路由排除原因（最多 128 项）</p>${rejections.map(item => `<div class="notice"><strong>${esc(item.model)}</strong><pre>${esc(item.reason)}</pre></div>`).join('')}`;
}
/** 展示请求元信息与错误分类，不渲染错误为 HTML。 */
function requestDetail(id) {
  const r = state.records.find((item) => item.id === id);
  if (!r) return;
  modal('请求详情', `<p class="muted">${esc(modelLabel(r.resolved_model || r.requested_model) || '未解析模型')} · ${requestTime(r.at)}</p><dl class="detail-summary"><div><dt>协议 / 结果</dt><dd>${esc(r.protocol)} · ${r.stream ? '流式' : '非流式'} · ${requestOutcome(r)[0]}</dd></div><div><dt>HTTP / 上游错误状态</dt><dd>${r.status || '待定'} / ${r.upstream_status ?? '—'}</dd></div><div><dt>总耗时 / 首事件</dt><dd>${number(r.duration_ms)} / ${number(r.first_event_ms)} ms</dd></div><div><dt>工具 / 输入</dt><dd>${r.tools} 个工具 / ${r.input_chars} 字符</dd></div><div><dt>Token 输入 / 输出</dt><dd>${number(r.usage?.input_tokens)} / ${number(r.usage?.output_tokens)}</dd></div><div><dt>终止原因</dt><dd>${esc(r.finish_reason || '—')}</dd></div></dl>${recordRequestScaleDetails(r)}<p class="hint">请求模型 → 实际模型</p><pre>${esc(r.requested_model || '—')} → ${esc(r.resolved_model || '—')}</pre><p class="hint">请求 ID</p><pre>${esc(r.id)}</pre><p class="hint">Trace / Span / Parent span</p><pre>${esc(r.trace_id || '—')}\n${esc(r.span_id || '—')}\n${esc(r.parent_span_id || '根请求')}</pre><div class="button-row">${r.trace_id ? `<button class="button small" data-action="request-trace" data-id="${esc(r.id)}">同 Trace 请求</button>` : ''}${r.session_id ? `<button class="button small" data-action="request-session" data-id="${esc(r.id)}">同会话请求</button>` : ''}</div>${recordSessionDetails(r)}${r.error ? `<p class="hint">错误分类</p><pre>${esc(r.error)}</pre>` : ''}${recordRouteRejections(r)}<p class="hint">上游尝试（请求 span 的子节点）</p>${(r.attempts || []).map(recordAttemptDetails).join('') || '<p class="muted">尚未调用上游。</p>'}<p class="hint">刷新记录后可看到进行中请求的最新状态。关联结果只包含当前保留范围内的请求。</p>`);
}
/** 展示已验证的本地图片，附件区同时提供移除入口。 */
function chatImageMarkup(images, removable = false) {
  return images.map((image) => `<figure class="play-image">${image.dataUrl ? `<button type="button" class="play-image-preview" data-action="preview-chat-image" aria-label="查看图片 ${esc(image.name)}"><img src="${esc(image.dataUrl)}" alt="${esc(image.name)}"></button>` : '<div class="play-image-loading"><span class="spinner"></span><span>读取中…</span></div>'}<figcaption title="${esc(image.name)}">${esc(image.name)}</figcaption>${removable ? `<button type="button" class="icon-button play-image-remove" data-action="remove-chat-image" data-id="${image.id}" aria-label="移除图片 ${esc(image.name)}" ${state.chat ? 'disabled' : ''}>${icon('close')}</button>` : ''}</figure>`).join('');
}
/** 在当前页面放大图片，避免浏览器阻止直接打开 data URL。 */
function previewChatImage(button) {
  const image = button.querySelector('img');
  if (image) modal('查看图片', `<img class="play-image-full" src="${esc(image.src)}" alt="${esc(image.alt)}"><p class="hint">${esc(image.alt)}</p>`);
}
/** 附件读取期间禁止提交，请求期间冻结会改变当前请求的控件。 */
function updateChatControls() {
  $('play-send').disabled = !!state.chat || state.chatImages.some((image) => !image.dataUrl);
  $('play-stop').hidden = !state.chat;
  for (const id of ['play-model', 'play-protocol', 'play-prompt', 'play-max', 'play-key', 'play-add-image', 'play-image-file']) $(id).disabled = !!state.chat;
  $('play-images').querySelectorAll('button').forEach((button) => { button.disabled = !!state.chat; });
}
/** 刷新附件预览和读取状态，不覆盖正在进行的请求状态。 */
function renderChatImages() {
  $('play-images').hidden = !state.chatImages.length;
  $('play-images').innerHTML = chatImageMarkup(state.chatImages, true);
  updateChatControls();
  if (!state.chat) {
    const pending = state.chatImages.filter((image) => !image.dataUrl).length;
    $('play-status').textContent = pending ? `正在读取 ${pending} 张图片…` : state.chatImages.length ? `已添加 ${state.chatImages.length} 张图片` : '准备就绪';
  }
}
/** 移除附件并取消读取；异步解码完成后不能恢复已移除的图片。 */
function removeChatImage(id) {
  if (state.chat) return;
  const image = state.chatImages.find((item) => item.id === id);
  state.chatImages = state.chatImages.filter((item) => item !== image);
  image?.reader.abort();
  renderChatImages();
}
/** 清空所有图片，释放本地数据并取消尚未完成的读取。 */
function clearChatImages() {
  const images = state.chatImages;
  state.chatImages = [];
  images.forEach((image) => image.reader.abort());
  $('play-image-file').value = '';
  renderChatImages();
}
/** 读取并解码真实图片，限制格式、单图大小与数量，文件只保留在页面内存。 */
function addChatImages(files) {
  if (state.chat) { toast('请先停止当前请求再添加图片'); return; }
  const types = { jpg: 'image/jpeg', jpeg: 'image/jpeg', png: 'image/png', webp: 'image/webp', gif: 'image/gif' };
  const errors = [];
  for (const file of Array.from(files || [])) {
    const mimeType = file.type || types[file.name?.split('.').pop().toLowerCase()];
    if (!Object.values(types).includes(mimeType)) { errors.push(`${file.name || '文件'}：仅支持 JPEG、PNG、WebP、GIF`); continue; }
    if (!file.size || file.size > 10 * 1024 * 1024) { errors.push(`${file.name}：图片需大于 0 且不超过 10 MiB`); continue; }
    if (state.chatImages.length >= 8) { errors.push('最多添加 8 张图片'); break; }
    const reader = new FileReader();
    const image = { id: ++state.chatImageId, name: file.name || `图片 ${state.chatImageId}`, mimeType, dataUrl: '', reader };
    state.chatImages.push(image);
    /** 丢弃无效附件，但不影响同批次的其他图片。 */
    const fail = () => {
      if (!state.chatImages.includes(image)) return;
      removeChatImage(image.id);
      toast(`${image.name}：无法读取或解码图片，请选择有效的图片文件`, true);
    };
    reader.onerror = fail;
    /** 确认图片可解码后才允许发送，并检查读取期间是否已被移除。 */
    reader.onload = async () => {
      if (!state.chatImages.includes(image)) return;
      try {
        const result = reader.result;
        if (typeof result !== 'string' || !result.includes(';base64,')) { fail(); return; }
        const dataUrl = `data:${mimeType};base64,${result.split(',')[1]}`;
        const decoded = new Image();
        decoded.src = dataUrl;
        await decoded.decode();
        if (!state.chatImages.includes(image)) return;
        image.dataUrl = dataUrl;
        renderChatImages();
      } catch { fail(); }
    };
    try { reader.readAsDataURL(file); } catch { fail(); }
  }
  renderChatImages();
  if (errors.length) toast([...new Set(errors)].join('；'), true);
}
/** 绑定文件选择、图片粘贴和文件拖拽；混合粘贴同时保留文本。 */
function initChatImages() {
  $('play-add-image').addEventListener('click', () => $('play-image-file').click());
  $('play-image-file').addEventListener('change', (event) => {
    addChatImages(event.target.files);
    event.target.value = '';
  });
  $('play-prompt').addEventListener('paste', (event) => {
    const files = Array.from(event.clipboardData?.items || []).filter((item) => item.kind === 'file').map((item) => item.getAsFile()).filter(Boolean);
    if (!files.length) return;
    event.preventDefault();
    if (state.chat) return;
    const text = event.clipboardData.getData('text/plain');
    if (text) $('play-prompt').setRangeText(text, $('play-prompt').selectionStart, $('play-prompt').selectionEnd, 'end');
    addChatImages(files);
  });
  const composer = $('play-composer');
  let dragDepth = 0;
  composer.addEventListener('dragenter', (event) => {
    if (!Array.from(event.dataTransfer?.types || []).includes('Files')) return;
    event.preventDefault();
    dragDepth++;
    composer.classList.toggle('drag-over', !state.chat);
  });
  composer.addEventListener('dragover', (event) => {
    if (!Array.from(event.dataTransfer?.types || []).includes('Files')) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = state.chat ? 'none' : 'copy';
  });
  composer.addEventListener('dragleave', () => {
    dragDepth = Math.max(0, dragDepth - 1);
    if (!dragDepth) composer.classList.remove('drag-over');
  });
  composer.addEventListener('drop', (event) => {
    if (!Array.from(event.dataTransfer?.types || []).includes('Files')) return;
    event.preventDefault();
    dragDepth = 0;
    composer.classList.remove('drag-over');
    addChatImages(event.dataTransfer?.files);
  });
}
/** 按协议编码单轮图文请求；纯文本保持原来的请求结构。 */
function requestBody(protocol, model, prompt, maxTokens, images = []) {
  if (images.length) {
    if (protocol === 'responses') return { model, input: [{ type: 'message', role: 'user', content: [
      ...(prompt ? [{ type: 'input_text', text: prompt }] : []),
      ...images.map((image) => ({ type: 'input_image', image_url: image.dataUrl })),
    ] }], max_output_tokens: maxTokens };
    const content = [
      ...(prompt ? [{ type: 'text', text: prompt }] : []),
      ...images.map((image) => protocol === 'messages'
        ? { type: 'image', source: { type: 'base64', media_type: image.mimeType, data: image.dataUrl.split(',')[1] } }
        : { type: 'image_url', image_url: { url: image.dataUrl } }),
    ];
    return { model, messages: [{ role: 'user', content }], max_tokens: maxTokens };
  }
  if (protocol === 'responses') return { model, input: prompt, max_output_tokens: maxTokens };
  return { model, messages: [{ role: 'user', content: prompt }], max_tokens: maxTokens };
}
/** 提交一次受用户控制的推理请求，输出文本与真实用量。 */
async function sendChat(event) {
  event.preventDefault();
  if (state.chat) return;
  const prompt = $('play-prompt').value.trim();
  const images = state.chatImages.slice();
  if (!prompt && !images.length) { toast('请输入消息或添加图片'); $('play-prompt').focus(); return; }
  if (images.some((image) => !image.dataUrl)) { toast('请等待图片读取完成'); return; }
  const model = $('play-model').value;
  const protocol = $('play-protocol').value;
  const body = requestBody(protocol, model, prompt, Number($('play-max').value), images);
  const bodyLimit = state.runtime?.max_request_body_bytes || 64 * 1024 * 1024;
  if (new Blob([JSON.stringify(body)]).size > bodyLimit) {
    toast(`请求超过服务端 ${number(bodyLimit / 1024 / 1024)} MiB 上限，请减少图片或缩小文件`, true);
    return;
  }
  const controller = new AbortController();
  state.chat = controller;
  updateChatControls();
  $('play-status').textContent = '正在请求上游…';
  $('play-output').innerHTML = `<div class="play-user">${prompt ? `<div>${esc(prompt)}</div>` : ''}${images.length ? `<div class="play-images play-sent-images">${chatImageMarkup(images)}</div>` : ''}</div><div class="play-answer" id="play-answer"><span class="spinner" style="display:inline-block"></span></div><div class="play-result-meta" id="play-result-meta" hidden></div>`;
  const started = performance.now();
  try {
    const path = protocol === 'chat' ? '/v1/chat/completions' : `/v1/${protocol}`;
    const result = await api(path, { method: 'POST', body, inference: true, signal: controller.signal, timeout: 180000 });
    const text = protocol === 'chat' ? result.choices?.[0]?.message?.content
      : protocol === 'messages' ? result.content?.filter((p) => p.type === 'text').map((p) => p.text).join('\n')
      : result.output?.flatMap((item) => item.content || []).filter((p) => p.type === 'output_text').map((p) => p.text).join('\n');
    $('play-answer').textContent = text || '上游未返回文本；请查看响应结构。';
    const usage = result.usage || {};
    const input = usage.prompt_tokens ?? usage.input_tokens;
    const output = usage.completion_tokens ?? usage.output_tokens;
    $('play-result-meta').hidden = false;
    $('play-result-meta').textContent = `${((performance.now() - started) / 1000).toFixed(2)} s · 输入 ${number(input)} / 输出 ${number(output)} Token · ${protocol}`;
    const details = document.createElement('details');
    details.innerHTML = `<summary>查看响应 JSON</summary><pre>${esc(JSON.stringify(result, null, 2))}</pre>`;
    details.className = 'play-advanced';
    $('play-output').append(details);
    $('play-status').textContent = '请求已完成';
  } catch (error) {
    $('play-answer').textContent = controller.signal.aborted ? '请求已停止。' : error.message;
    $('play-answer').style.color = controller.signal.aborted ? '' : 'var(--danger)';
    $('play-status').textContent = controller.signal.aborted ? '已停止' : '请求失败';
  } finally {
    state.chat = null;
    updateChatControls();
    refresh().catch((error) => toast(error.message, true));
  }
}
/** 客户端只展示一个自动入口，历史名称由服务端兼容。 */
function autoOptions() {
  return '<option value="auto">Auto · 自动额度池</option>';
}
/** 使用共同的模型预算与账号候选预演当前策略，不发起生成。 */
function routeModal() {
  modal('预演自动路由', `<p class="muted">Auto 自动额度池：查看费用与额度分层、优先级及上下文准入结果。</p><form id="route-form"><div class="form-grid"><label>输入 Token 估算<input id="route-input" type="number" min="0" max="2000000" value="0"></label><label>输出 Token 预留<input id="route-output" type="number" min="1" value="8192"></label><label>需要工具调用<select id="route-tools"><option value="false">不要求</option><option value="true">需要</option></select></label><label>需要图片输入<select id="route-images"><option value="false">不要求</option><option value="true">需要</option></select></label></div><p class="hint">按层级与优先级选池，同域比较倍率，同级按负载和会话分散。付费后备在网关设置中控制。</p><button class="button primary" id="route-submit">查看候选</button></form><div id="route-result" class="route-result"></div>`);
  $('route-form').addEventListener('submit', (e) => { e.preventDefault(); busy($('route-submit'), async () => {
    const query = new URLSearchParams({ model: 'auto', tools: $('route-tools').value, images: $('route-images').value, input_tokens: $('route-input').value, output_tokens: $('route-output').value });
    const result = await api(`/api/route/plan?${query}`);
    $('route-result').innerHTML = `<h3>${result.candidates.length} 个候选</h3><p class="hint">${esc(result.comparison_note)}</p>${result.candidates.map((c, i) => `<div class="route-item">${i + 1}. ${esc(modelLabel(c.backend + '/' + c.model))}<small>${c.pool === 'quota' ? '免费 / 套餐池' : c.pool === 'paid' ? '付费后备' : '费用未知后备'} · ${c.quota_known ? '额度已确认' : '额度未知候补'} · 优先级 ${c.priority}<br>倍率 ${c.price_factor == null ? '未知' : c.price_factor + '×'} · 余额 ${c.quota.unlimited ? '不限量' : esc(c.quota.complete ? c.quota.remaining ?? '未知' : '未知')} ${esc(c.quota.unit)}<br>账号 ${esc(state.accounts.find((a) => a.id === c.account_id)?.name || c.account_id || '网关')} · 比较范围 ${esc(c.comparison_scope)}<br>输入估算 ${number(c.limits.estimated_input)} + 余量 ${number(c.limits.input_margin)} · 输出预留 ${number(c.limits.output_reserve)} · 输入上限 ${c.limits.input_limit == null ? '未知' : number(c.limits.input_limit)} · 窗口 ${c.limits.context_limit == null ? '未知' : number(c.limits.context_limit)}</small></div>`).join('') || '<p class="hint">当前没有符合条件的服务。检查服务的费用类别、额度与上下文配置。</p>'}${result.rejected.length ? `<details open><summary class="hint">${result.rejected.length} 个排除项</summary>${result.rejected.map((r) => `<div class="route-item">${esc(modelLabel(r.model))}<small>${esc(r.reason)}</small></div>`).join('')}</details>` : ''}`;
  }); });
}
/** 确认签到的服务范围，执行后展示接口实际返回的结果。 */
function checkin(id) {
  const b = state.backends.find((item) => item.id === id);
  const label = dailyTaskLabel(b);
  modal(`执行每日${label}`, `<p>为 ${esc(b.name)} 的 ${accountsOf(id).filter((a) => a.enabled).length} 个启用账号执行${label}。</p><p class="hint">${b.kind === 'raccoon' ? '访问官网登录接口，并核对今天发放的积分是否保留至含发放日的第七天。无法确认时记录结果未知。' : b.kind === 'trae_personal' ? '先查询个人账号今日签到状态，尚未签到时领取积分。结果未知会保留记录并停止重放，请在个人客户端核对。' : '将向上游查询并领取可用奖励，结果会写入任务记录。'}</p><div class="actions"><button class="button" data-action="close-modal">取消</button><button class="button primary" id="run-checkin">执行${label}</button></div>`);
  $('run-checkin').addEventListener('click', () => busy($('run-checkin'), async () => {
    await runPluginOperation(id, 'checkin');
  }));
}
/** 专用地址选择 FMP 内的客户端插件，客户端只需使用已有 Base URL 配置。 */
function integrationBaseUrl() {
  const client = $('example-client').value;
  const prefix = { trae: '/clients/trae', minimax_code: '/clients/minimax-code' }[client] || '';
  return location.origin + prefix + '/v1';
}
/** 使用真实模型生成可复制的 cURL 示例；密钥用环境变量替代。 */
function renderExample() {
  const protocol = $('example-protocol').value;
  const base = integrationBaseUrl();
  const path = protocol === 'chat' ? '/chat/completions' : `/${protocol}`;
  $('settings-endpoint').textContent = base;
  $('client-session-hint').textContent = {
    trae: '填入 TRAE 现有 Base URL 配置。FMP 按完整历史延续推断分组，请求记录标明依据；没有读取 TRAE 原生 SessionID。',
    minimax_code: '填入 MiniMax Code 现有 Base URL 配置。使用客户端自带的原生会话 ID 分组；通用 /v1 地址也能自动识别。',
  }[$('example-client').value] || '通用地址自动识别 MiniMax Code 原生会话。TRAE 可选择专用地址启用历史关联。';
  const body = JSON.stringify(requestBody(protocol, $('example-model').value || 'auto', '你好', 256), null, 2);
  const quoted = body.replace(/'/g, "'\\''");
  $('example-code').textContent = `curl '${base}${path}' \\\n  -H 'Content-Type: application/json' \\\n${state.runtime?.api_key_required ? '  -H "Authorization: Bearer $FMP_API_KEY" \\\n' : ''}  -d '${quoted}'`;
  renderSelectedLimits('example-model', 'example-limits');
}
/** 复制文本，权限不可用时展示可选中的内容。 */
async function copy(text) {
  try { await navigator.clipboard.writeText(text); toast('已复制'); }
  catch { modal('复制内容', `<p class="hint">当前浏览器未允许写入剪贴板，可手动选择以下内容。</p><textarea rows="5" readonly>${esc(text)}</textarea>`); }
}
/** 下载只含脱敏配置的文件。 */
async function exportConfig() {
  const data = await api('/api/config/export');
  const url = URL.createObjectURL(new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' }));
  const a = document.createElement('a');
  a.href = url;
  a.download = 'fakemodelproxy-config.json';
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
  toast('脱敏配置已导出，不含密钥');
}
/** 先预览配置文件的合并范围，再提交服务端验证和导入。 */
async function previewImport(file) {
  if (!file) return;
  if (file.size > 2 * 1024 * 1024) throw new Error('配置文件不能超过 2 MB');
  let bundle;
  try { bundle = JSON.parse(await file.text()); } catch { throw new Error('无法解析 JSON 配置文件'); }
  if (!bundle || !Array.isArray(bundle.backends) || !Array.isArray(bundle.accounts)) throw new Error('文件需包含 backends 和 accounts 数组');
  state.importBundle = bundle;
  const replace = bundle.backends.filter((b) => state.backends.some((current) => current.id === b.id)).length;
  modal('预览配置导入', `<p class="muted">${esc(file.name)}</p><div class="import-summary"><span>${bundle.backends.length} 个服务</span><span>${bundle.accounts.length} 个账号</span></div><p class="hint">其中 ${replace} 个服务 ID 已存在，将合并覆盖配置。空密钥保留现有值。</p><div class="actions"><button class="button" data-action="close-modal">取消</button><button class="button primary" id="confirm-import">确认导入</button></div>`);
  $('confirm-import').addEventListener('click', () => busy($('confirm-import'), async () => {
    const result = await api('/api/config/import', { method: 'POST', body: state.importBundle });
    $('modal').close();
    await refresh();
    toast(`已导入 ${result.backends} 个服务、${result.accounts} 个账号`);
  }));
}
/** 网关字段单位直接显示在标签中，与管理 API 使用同一组设置键。 */
function gatewayFields() {
  return [
    ['max_attempts', '单请求尝试上限', 1, 16],
    ['total_timeout_ms', '请求总时限（毫秒）', 1, 86400000],
    ['connect_timeout_ms', '连接时限（毫秒）', 1, 86400000],
    ['startup_timeout_ms', '启动时限（毫秒）', 1, 86400000],
    ['idle_timeout_ms', '流空闲时限（毫秒）', 1, 86400000],
    ['backoff_base_ms', '退避基数（毫秒）', 0, 60000],
    ['backoff_cap_ms', '退避上限（毫秒）', 0, 60000],
    ['queue_timeout_ms', '容量与 QPS 等待（毫秒）', 1, 86400000],
    ['queue_capacity', '等待队列容量', 1, 100000],
    ['group_ttl_ms', '分组保留时间（毫秒）', 1, 86400000],
    ['group_capacity', '分组容量', 1, 100000],
  ];
}
/** 刷新计数时保留用户未保存的表单，运行状态仅显示组数和目标负载。 */
function renderGateway() {
  const settings = state.gateway?.settings;
  const node = $('gateway-runtime');
  if (!node) return;
  if (!settings) { node.textContent = '正在读取网关设置'; return; }
  if (!state.gatewayDirty && !state.gatewaySaving) {
    $('gateway-paid-fallback').checked = settings.auto_allow_paid_fallback;
    $('gateway-fields').innerHTML = gatewayFields().map(([key, label, min, max]) =>
      `<label>${esc(label)}<input id="gateway-${key}" type="number" min="${min}" max="${max}" step="1" required value="${settings[key]}"></label>`).join('');
    $('gateway-targets').value = JSON.stringify(settings.targets || {}, null, 2);
  }
  const runtime = state.gateway.runtime || {};
  const balancer = runtime.balancer || {};
  const loads = balancer.loads || [];
  const cooldowns = balancer.cooldowns || [];
  node.innerHTML = `<p>当前分组 ${number(balancer.groups)} · 等待 ${number(runtime.waiting)}</p>
    ${loads.length ? `<div class="table-wrap"><table><thead><tr><th>服务 / 账号</th><th>在飞请求</th><th>分组</th></tr></thead><tbody>${loads.map((row) => `<tr><td>${esc(row.target)}</td><td>${number(row.in_flight)}</td><td>${number(row.active_groups)}</td></tr>`).join('')}</tbody></table></div>` : '<p>尚无运行负载。</p>'}
    ${cooldowns.map((row) => `<p>${esc(row.scope)} · ${esc(row.reason)} · 恢复等待 ${Math.ceil(row.remaining_ms / 1000)} 秒</p>`).join('')}`;
}
/** 校验整数字段后保存完整设置，失败时保留输入供修正。 */
async function saveGateway(event) {
  event.preventDefault();
  if (!state.gateway?.settings || state.gatewaySaving) return;
  await busy(event.submitter, async () => {
    const settings = { ...state.gateway.settings, auto_allow_paid_fallback: $('gateway-paid-fallback').checked };
    for (const [key, label, min, max] of gatewayFields()) {
      const value = Number($(`gateway-${key}`).value);
      if (!Number.isSafeInteger(value) || value < min || value > max) throw new Error(`${label}需为 ${min} 至 ${max} 的整数`);
      settings[key] = value;
    }
    try { settings.targets = JSON.parse($('gateway-targets').value || '{}'); }
    catch { throw new Error('容量与权重覆盖需为有效 JSON'); }
    if (!settings.targets || Array.isArray(settings.targets) || typeof settings.targets !== 'object') throw new Error('容量与权重覆盖需为 JSON 对象');
    const signature = formSignature($('gateway-form'));
    state.gatewaySaving = true;
    try {
      state.gateway = await api('/api/gateway', { method: 'PUT', body: settings });
      state.gatewayDirty = signature !== formSignature($('gateway-form'));
      toast('网关设置已保存并生效');
    } finally { state.gatewaySaving = false; renderGateway(); fillModelSelectors(); }
  });
}
/** 将页面动作分派到真实操作；按钮范围通过 DOM 委托保持一致。 */
function handleAction(event, discardConfirmed = false) {
  const button = event.target.closest('[data-action]');
  if (!button) return;
  const { action, id } = button.dataset;
  const replacesDrawer = ['add-provider', 'edit-provider', 'provider', 'model-detail', 'select-model', 'try-provider'].includes(action);
  const replacesModal = ['add-account', 'edit-account', 'delete-account', 'delete-provider', 'login', 'request-detail', 'route', 'checkin', 'import'].includes(action);
  if (!discardConfirmed && (replacesDrawer && !allowDiscard($('drawer'), () => handleAction(event, true)) || replacesModal && !allowDiscard($('modal'), () => handleAction(event, true)))) return;
  const actions = {
    refresh: () => busy(button, refresh),
    quota: () => busy(button, refreshQuota),
    'gateway-status': () => busy(button, async () => { state.gateway = await api('/api/gateway'); renderGateway(); }),
    'add-provider': () => editProvider(),
    'edit-provider': () => editProvider(id),
    'config-capture': () => busy(button, () => captureConfigHelper(Number(id))),
    'config-import-file': () => $(`config-file-${id}`)?.click(),
    'config-import-json': () => busy(button, () => importConfigHelper(Number(id))),
    'config-use-candidate': () => busy(button, () => useConfigHelperCandidate(Number(id))),
    provider: () => openProvider(id),
    'provider-models': () => { location.hash = 'models'; $('model-search').value = ''; $('model-provider').value = id; saveModelGroup(id, true); renderModels(); window.SelectControls?.refresh(); },
    'try-provider': () => { const b = state.backends.find((r) => r.id === id); selectModel(`${id}/${modelsOf(b)[0]}`); },
    'select-model': () => selectModel(id),
    'model-detail': () => openModelDetails(id),
    'copy-call-name': () => copy(id),
    'copy-model-params': () => { const reference = modelConfigReference(id); if (reference) return copy(JSON.stringify(reference, null, 2)); },
    'sync-models': () => busy(button, () => syncModels(id)),
    'login-sync': () => busy(button, async () => { await syncModels(id); $('modal').close(); }),
    'autoglm-browser': () => startBrowserLogin(id, button.dataset.provider),
    'toggle-provider': () => busy(button, () => patchProvider(id, { enabled: !state.backends.find((b) => b.id === id).enabled })),
    'delete-provider': () => confirmDelete('provider', id),
    'add-account': () => editAccount('', id),
    'edit-account': () => editAccount(id),
    'delete-account': () => confirmDelete('account', id),
    'refresh-account': () => busy(button, async () => {
      const account = state.accounts.find((item) => item.id === id);
      await runPluginOperation(account.backend_id, 'credential.refresh', { account_id: id });
    }),
    'plugin-operation': () => button.dataset.operation === 'checkin' ? checkin(id) : busy(button, () => requestPluginOperation(id, button.dataset.operation)),
    operation: () => busy(button, () => openOperation(id)),
    'cancel-operation': () => busy(button, async () => {
      await api(`/api/v2/operations/${encodeURIComponent(id)}/cancel`, { method: 'POST', body: {} });
      await openOperation(id);
    }),
    'apply-plan': () => busy(button, applyConfigPlan),
    login: () => login(id),
    'request-detail': () => requestDetail(id),
    'request-trace': () => requestGroup('trace_id', state.records.find((r) => r.id === id)?.trace_id),
    'request-session': () => requestGroup('session_id', state.records.find((r) => r.id === id)?.session_id),
    'request-ungroup': () => requestGroup(null, null),
    checkin: () => checkin(id),
    'checkin-all': () => checkinAll(button),
    route: routeModal,
    'close-drawer': () => { if (allowDiscard($('drawer'), () => $('drawer').close())) $('drawer').close(); },
    'close-modal': () => { if (allowDiscard($('modal'), () => $('modal').close())) $('modal').close(); },
    'keep-editing': () => $('discard-dialog').close(),
    'discard-editing': () => {
      const action = state.discardAction;
      $('discard-dialog').close();
      state.discardAction = null;
      action?.();
    },
    'models-expand': () => expandModelGroups(true),
    'models-collapse': () => expandModelGroups(false),
    'reset-models': () => { $('model-search').value = ''; $('model-provider').value = ''; renderModels(); window.SelectControls?.refresh(); },
    'reset-providers': () => { $('provider-search').value = ''; $('provider-filter').value = 'all'; renderProviders(); window.SelectControls?.refresh(); },
    'reset-requests': () => requestGroup(null, null),
    'copy-endpoint': () => copy(button.dataset.target === 'settings' ? integrationBaseUrl() : location.origin + '/v1'),
    'copy-model': () => copy($('play-model').value),
    'copy-example': () => copy($('example-code').textContent),
    'fill-prompt': () => { $('play-prompt').value = '只回复：你好，连接成功。'; $('play-prompt').focus(); },
    'clear-chat': () => { if (state.chat) { toast('请先停止当前请求'); return; } $('play-output').innerHTML = '<div class="play-empty"><h3>开始一次新的测试</h3><p>每次请求独立发送。</p></div>'; $('play-prompt').value = ''; clearChatImages(); },
    'remove-chat-image': () => removeChatImage(Number(button.dataset.id)),
    'preview-chat-image': () => previewChatImage(button),
    export: () => busy(button, exportConfig),
    import: () => $('import-file').click(),
  };
  actions[action]?.();
}
/** 记录字段值，用户恢复原值时不再提示未保存；快照仅保存在 DOM 内存。 */
function formSignature(form) {
  return JSON.stringify([...form.elements].filter((field) => ['INPUT', 'SELECT', 'TEXTAREA'].includes(field.tagName))
    .map((field) => [field.id, field.type === 'checkbox' ? field.checked : field.value]));
}
/** 给可编辑表单标记基线，程序保存后的关闭不经过用户关闭保护。 */
function trackDraft(form) {
  form.dataset.draft = formSignature(form);
}
/** 关闭或替换编辑界面前保护尚未保存的字段。 */
function allowDiscard(dialog, onDiscard) {
  if (!dialog.open) return true;
  const form = dialog.querySelector('form[data-draft]');
  if (!form || form.dataset.draft === formSignature(form)) return true;
  state.discardAction = onDiscard || null;
  $('discard-dialog').showModal();
  return false;
}
/** 加载本地额度快照，外部额度由后台任务或手动更新读取。 */
function init() {
  hydrateIcons();
  document.addEventListener('click', handleAction);
  window.addEventListener('hashchange', navigate);
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') {
      fillModelSelectors(); renderModels(); renderProviders(); window.SelectControls?.refresh();
      if (state.drawerProvider && $('drawer').open) openProvider(state.drawerProvider, false);
      schedulePriceExpiry();
      refreshQuotaCache().catch(() => {}).finally(scheduleQuotaPoll);
    } else {
      clearTimeout(state.quotaTimer);
      state.quotaTimer = null;
    }
  });
  $('provider-search').addEventListener('input', renderProviders);
  $('provider-filter').addEventListener('change', renderProviders);
  $('model-search').addEventListener('input', renderModels);
  $('model-provider').addEventListener('change', renderModels);
  $('play-model').addEventListener('change', () => {
    const model = modelAt($('play-model').value);
    if (model) saveModelGroup(model.backend.id, true);
    renderModels();
  });
  $('request-search').addEventListener('input', renderRequests);
  $('request-filter').addEventListener('change', renderRequests);
  $('request-protocol').addEventListener('change', renderRequests);
  $('example-protocol').addEventListener('change', renderExample);
  $('example-client').addEventListener('change', renderExample);
  $('example-model').addEventListener('change', renderExample);
  $('play-form').addEventListener('submit', sendChat);
  initChatImages();
  $('play-prompt').addEventListener('keydown', (e) => { if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') { e.preventDefault(); $('play-form').requestSubmit(); } });
  $('play-stop').addEventListener('click', () => state.chat?.abort());
  $('schedule-form').addEventListener('input', () => { state.scheduleDirty = true; renderScheduleWindow(); });
  $('schedule-form').addEventListener('submit', saveSchedule);
  $('gateway-form').addEventListener('input', () => { state.gatewayDirty = true; });
  $('gateway-form').addEventListener('submit', saveGateway);
  $('admin-form').addEventListener('submit', (e) => { e.preventDefault(); state.adminKey = $('admin-key').value.trim(); busy(e.submitter, async () => { await refresh(); toast('控制台已连接'); }); });
  $('modal').addEventListener('close', () => { window.SelectControls?.close(); cancelLogin(); state.importBundle = null; });
  $('drawer').addEventListener('close', () => { window.SelectControls?.close(); state.drawerProvider = null; });
  for (const id of ['modal', 'drawer']) $(id).addEventListener('cancel', (event) => {
    event.preventDefault();
    if (allowDiscard($(id), () => $(id).close())) $(id).close();
  });
  $('discard-dialog').addEventListener('close', () => { state.discardAction = null; });
  $('import-file').addEventListener('change', (e) => { busy(null, () => previewImport(e.target.files[0])); e.target.value = ''; });
  navigate();
  render();
  refresh().catch((error) => toast(error.message, true));
}
init();
