const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const ts = require(process.env.UU_TYPESCRIPT_PATH ||
  '/Applications/DevEco-Studio.app/Contents/tools/hvigor/hvigor/node_modules/typescript');
const root = path.resolve(__dirname, '../..');

function harness({ addError = false, removeError = false, deferAdd = false, language = 'zh-CN' } = {}) {
  const events = new Map();
  const windows = new Map();
  const calls = [];
  let finishAdd;
  let trayItem;
  const context = {
    resourceManager: { getMediaContentSync: () => new Uint8Array([0]) },
    hideAbility: async () => { calls.push('hide'); },
    showAbility: async () => { calls.push('show'); },
    terminateSelf: async () => { calls.push('terminate'); },
  };
  const native = { stop: () => calls.push('stop') };
  const statusBarManager = {
    on: (event, callback) => events.set(event, callback),
    off: (event, callback) => {
      if (events.get(event) === callback) events.delete(event);
    },
    addToStatusBar: (_, item, callback) => {
      trayItem = item;
      assert.equal(item.quickOperation.abilityName, '');
      calls.push('add');
      finishAdd = () => callback(addError ? { code: 801 } : undefined);
      if (!deferAdd) finishAdd();
    },
    removeFromStatusBar: (_, callback) => {
      calls.push('remove');
      callback(removeError ? { code: 801 } : undefined);
    },
  };
  const image = {
    createImageSource: () => ({
      createPixelMap: async () => ({ release: () => calls.push('release icon') }),
      release: () => calls.push('release source'),
    }),
  };
  class UIAbility { constructor() { this.context = context; } }
  const modules = {
    '@kit.AbilityKit': { UIAbility },
    '@kit.ArkUI': {},
    '@kit.BasicServicesKit': {},
    '@kit.DeskTopExtensionKit': { statusBarManager },
    '@kit.ImageKit': { image },
    '@kit.LocalizationKit': { i18n: { System: { getSystemLanguage: () => language } } },
    '@kit.PerformanceAnalysisKit': { hilog: { info() {}, error() {} } },
    'libentry.so': native,
  };
  function load(relative) {
    const source = fs.readFileSync(path.join(root, relative), 'utf8');
    const output = ts.transpileModule(source, {
      compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.CommonJS },
      fileName: path.basename(relative).replace(/\.ets$/, '.ts'),
    }).outputText;
    const module = { exports: {} };
    vm.runInNewContext(output, {
      module, exports: module.exports,
      require(name) {
        assert.ok(modules[name], `Unexpected import: ${name}`);
        return modules[name];
      },
      $r: () => ({ id: 1 }),
    }, { filename: relative });
    return module.exports;
  }
  modules['../common/Strings'] = load('harmony/entry/src/main/ets/common/Strings.ets');
  const trayModule = load('harmony/entry/src/main/ets/desktop/TrayController.ets');
  modules['../desktop/TrayController'] = trayModule;
  const { default: EntryAbility } = load('harmony/entry/src/main/ets/entryability/EntryAbility.ets');
  const ability = new EntryAbility();
  const stage = {
    getMainWindowSync: () => ({
      on: (event, callback) => windows.set(event, callback),
      minimize: async () => calls.push('minimize'),
    }),
    loadContent: (_, callback) => callback({ code: 0 }),
  };
  return {
    calls, events, ability,
    start: () => ability.onWindowStageCreate(stage),
    close: () => windows.get('windowWillClose')(),
    finishAdd: () => finishAdd(),
    menu: code => events.get('rightMenuClick')({ data: { menuCode: code } }),
    trayItem: () => trayItem,
  };
}
const flush = () => new Promise(resolve => setImmediate(resolve));

test('tray labels follow the system language in Chinese and English', async () => {
  for (const [language, label] of [['zh-CN', '打开主窗口'], ['en-US', 'Open main window']]) {
    const app = harness({ language });
    app.start();
    await flush();
    assert.equal(app.trayItem().statusBarGroupMenu[0][0].title, label);
    app.ability.onDestroy();
    await flush();
  }
});

test('closing to tray and reopening preserve sharing; only Stop or Exit stop it', async () => {
  const app = harness();
  app.start();
  await flush();
  assert.equal(await app.close(), true);
  assert.ok(app.calls.includes('hide'));
  assert.equal(app.calls.includes('stop'), false);
  app.events.get('statusBarIconClick')();
  await flush();
  assert.ok(app.calls.includes('show'));
  assert.equal(app.calls.includes('stop'), false);
  app.menu('stop');
  assert.equal(app.calls.filter(c => c === 'stop').length, 1);
  assert.equal(app.calls.includes('terminate'), false);
  app.menu('exit');
  await flush();
  assert.equal(app.calls.filter(c => c === 'stop').length, 2);
  assert.ok(app.calls.indexOf('remove') < app.calls.indexOf('terminate'));
  assert.equal(await app.close(), false);
  app.ability.onDestroy();
  await flush();
  assert.equal(app.events.size, 0);
});

test('tray failure retains a Dock restore path without ending the session', async () => {
  const app = harness({ addError: true });
  app.start();
  await flush();
  assert.equal(await app.close(), true);
  assert.ok(app.calls.includes('minimize'));
  assert.equal(app.calls.includes('stop'), false);
  assert.equal(app.calls.includes('hide'), false);
  assert.equal(app.events.size, 0);
  assert.equal(app.calls.filter(c => c === 'release icon').length, 2);
});

test('Exit still terminates when removing the status bar icon fails', async () => {
  const app = harness({ removeError: true });
  app.start();
  await flush();
  app.menu('exit');
  await flush();
  assert.ok(app.calls.includes('stop'));
  assert.ok(app.calls.includes('terminate'));
});

test('destroying during tray registration removes the late icon and listeners', async () => {
  const app = harness({ deferAdd: true });
  app.start();
  await flush();
  app.ability.onDestroy();
  app.finishAdd();
  await flush();
  assert.equal(app.events.size, 0);
  assert.ok(app.calls.includes('remove'));
  assert.equal(app.calls.filter(c => c === 'release icon').length, 2);
});
