const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const html = fs.readFileSync('src/panel.html', 'utf8');
const elements = new Map();
function element(id) {
  if (!elements.has(id)) elements.set(id, {
    value: '', textContent: '', hidden: false, disabled: false,
    classList: { toggle() {} }, replaceChildren() {}, append() {},
    setAttribute() {}, querySelector() { return { disabled: false }; },
  });
  return elements.get(id);
}
const calls = [];
const context = vm.createContext({
  document: { getElementById: element, querySelectorAll: () => [], createElement: () => element(Symbol()) },
  setInterval() {}, setTimeout() {}, clearTimeout() {}, AbortController,
  fetch: async (url, options) => {
    calls.push({url, ...options});
    return { ok: true, json: async () => ({mode:'dry-run', enabled:false, killed:false,
      risk_limits:{max_position_native:1,daily_stop_net_native:2,max_open_positions:3},
      fee_fraction:0.003,estimated_slippage_fraction:0.005, ledgers:{}}) };
  },
});
vm.runInContext(html.match(/<script>([\s\S]*?)<\/script>/)[1], context);
(async () => {
  assert(html.includes('<option value="gemini">Gemini</option>'), 'Gemini provider available');
  assert(html.includes('id="strategy-engine"'), 'Independent strategy section exists');
  await vm.runInContext('loadEngine()', context);
  assert.equal(element('engine-state').textContent, 'OFF · DRY RUN');
  assert.equal(element('engine-enable').disabled, false);
  vm.runInContext("network='elysium';renderEngine(null)", context);
  assert.equal(element('engine-controls').hidden, true);
  const before = calls.length;
  await vm.runInContext("engineAction('enabled',{enabled:true})", context);
  assert.equal(calls.length, before, 'Elysium cannot issue new engine actions');
  vm.runInContext("network='robinhood';connected=true", context);
  await vm.runInContext("engineAction('enabled',{enabled:false})", context);
  assert(calls.some(c=>c.url.includes('/api/panel/strategy/enabled?network=robinhood') && JSON.parse(c.body).enabled===false));
  for (const [id, value] of Object.entries({'engine-max-position':'0.25','engine-daily-stop':'0.1',
      'engine-max-open':'2','engine-fee':'0.003','engine-slippage':'0.005'})) element(id).value=value;
  element('engine-config').onsubmit({preventDefault(){}});
  await new Promise(resolve=>setImmediate(resolve));
  const configure=calls.find(c=>c.url.includes('/strategy/configure?network=robinhood'));
  assert.deepEqual(JSON.parse(configure.body), {max_position_native:0.25,daily_stop_net_native:0.1,
    max_open_positions:2,fee_fraction:0.003,estimated_slippage_fraction:0.005});
  vm.runInContext("renderEngine({enabled:false,killed:true,risk_limits:{}})", context);
  assert.equal(element('engine-enable').disabled, true);
  assert.equal(element('engine-tick').disabled, true);
  console.log('PASS: Gemini, OFF/DRY-RUN, Elysium guard, scoped actions, numeric risk/cost payload, killed-state controls');
})().catch(error => { console.error(error); process.exitCode=1; });
