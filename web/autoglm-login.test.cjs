/** 独立短信页面回归，所有 fetch 都由本地函数模拟，不联网。 */
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { test } = require('node:test');
const vm = require('node:vm');

/** 创建页面节点、同源 iframe 和可控制的网络结果，不自动运行初始化。 */
function fixture() {
  const nodes = new Map();
  const calls = [];
  const replies = new Map([
    ['status', { status: 'pending' }],
    ['captcha-config', { config: { enabled: false } }],
    ['send-code', { status: 'sent', retry_after: 60 }],
    ['login', { status: 'done', name: '测试账号' }],
    ['cancel', { status: 'cancelled' }],
  ]);
  const messages = [];
  const frameWindow = { postMessage: (message) => messages.push(message) };
  const timers = new Map();
  let timerId = 0;
  const context = vm.createContext({
    URL, URLSearchParams, AbortSignal,
    location: { origin: 'http://127.0.0.1:8317', pathname: '/auth/autoglm/test-session' },
    document: { getElementById: (id) => {
      if (!nodes.has(id)) nodes.set(id, {
        value: '', hidden: ['captcha-frame', 'retry', 'back'].includes(id),
        disabled: false, textContent: '', dataset: {}, contentWindow: frameWindow,
        addEventListener: () => {}, focus: () => {},
      });
      return nodes.get(id);
    } },
    window: { addEventListener: () => {} },
    setTimeout: (callback, delay) => { timers.set(++timerId, { callback, delay }); return timerId; },
    clearTimeout: (id) => timers.delete(id),
    fetch: async (url, options) => {
      const action = url.split('/').pop();
      calls.push({ action, body: JSON.parse(options.body) });
      const reply = replies.get(action);
      return { ok: !(reply instanceof Error), json: async () => reply instanceof Error ? {error:{message:reply.message}} : reply };
    },
  });
  const html = readFileSync(__dirname + '/autoglm-login.html', 'utf8');
  const source = html.match(/<script>([\s\S]*?)<\/script>/)[1].replace(/initialize\(\);\s*$/, '');
  vm.runInContext(source, context);
  return { context, nodes, calls, replies, frameWindow, messages, timers };
}

/** 首次加载只读取状态和公开配置；发送失败、换手机号时不能提交验证码。 */
test('sms submission unlocks only after confirmed sending to the current phone', async () => {
  const f = fixture();
  await vm.runInContext('initialize()', f.context);
  assert.deepEqual(f.calls.map((call) => call.action), ['status', 'captcha-config']);
  f.nodes.get('phone').value = '13800138000';
  f.nodes.get('code').value = '123456';
  vm.runInContext('updateControls()', f.context);
  assert.equal(f.nodes.get('submit').disabled, true);
  f.replies.set('send-code', new Error('未确认短信发送成功'));
  await vm.runInContext('sendCode()', f.context);
  assert.equal(f.nodes.get('submit').disabled, true);
  f.replies.set('send-code', { status: 'sent', retry_after: 60 });
  await vm.runInContext('sendCode()', f.context);
  f.nodes.get('code').value = '123456';
  vm.runInContext('updateControls()', f.context);
  assert.equal(f.nodes.get('submit').disabled, false);
  assert.equal(f.nodes.get('send').disabled, true);
  f.nodes.get('phone').value = '13900139000';
  vm.runInContext('updateControls()', f.context);
  assert.equal(f.nodes.get('submit').disabled, true);
  assert.equal(f.nodes.get('code').disabled, true);
});

/** 成功登录或取消后停止所有计时器，不把凭证留在页面。 */
test('saving and cancellation finish the page and stop polling', async () => {
  for (const cancel of [false, true]) {
    const f = fixture();
    await vm.runInContext('initialize()', f.context);
    if (cancel) {
      await vm.runInContext('cancelLogin()', f.context);
    } else {
      f.nodes.get('phone').value = '13800138000';
      vm.runInContext('updateControls()', f.context);
      await vm.runInContext('sendCode()', f.context);
      f.nodes.get('code').value = '123456';
      vm.runInContext('updateControls()', f.context);
      await vm.runInContext('submitLogin({preventDefault(){}})', f.context);
    }
    assert.equal(f.nodes.get('login-form').hidden, true);
    assert.equal(f.nodes.get('back').hidden, false);
    assert.equal(f.timers.size, 0);
    const before = f.calls.length;
    await vm.runInContext('pollStatus()', f.context);
    assert.equal(f.calls.length, before);
  }
});

/** 回执必须来自当前同源 iframe，业务成功只由代理发码响应决定。 */
test('captcha receipts are bound to the current iframe and upstream confirmation', async () => {
  const f = fixture();
  f.replies.set('captcha-config', {config:{
    enabled:true,captcha_supplier:'aliyun',region:'cn',prefix:'mock',scene_id:'mock-scene',
  }});
  await vm.runInContext('initialize()', f.context);
  f.nodes.get('phone').value = '13800138000';
  vm.runInContext('updateControls()', f.context);
  await vm.runInContext('sendCode()', f.context);
  assert.equal(f.nodes.get('captcha-frame').hidden, false);
  f.context.event = {origin:'https://other.example',source:f.frameWindow,data:{type:'fmp-captcha-proof',id:1,proof:'mock-proof'}};
  await vm.runInContext('receiveCaptcha(event)', f.context);
  f.context.event.origin = 'http://127.0.0.1:8317';
  f.context.event.source = {};
  await vm.runInContext('receiveCaptcha(event)', f.context);
  assert.equal(f.calls.filter((call) => call.action === 'send-code').length, 0);
  f.context.event.source = f.frameWindow;
  f.replies.set('send-code', new Error('安全验证未通过'));
  await vm.runInContext('receiveCaptcha(event)', f.context);
  assert.equal(f.messages.at(-1).accepted, false);
  assert.equal(f.nodes.get('code').disabled, true);
  f.replies.set('send-code', {status:'sent',retry_after:60});
  await vm.runInContext('receiveCaptcha(event)', f.context);
  assert.equal(f.messages.at(-1).accepted, true);
  assert.equal(f.nodes.get('code').disabled, false);
});
