// The loader's naming helpers on a synthetic export object shaped like jco's.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { findExport, listExports } from '../../dist/load.js';

class Counter {}
const store = { getAll: () => [], put: () => {}, Counter };
const run = { run: () => {} };
const exports = {
  _util: { stop: () => {} },
  appHello: () => 'hi',
  plain: () => 1,
  // jco keys an instance by its short name and by its full versioned name.
  appStore: store,
  'my:pkg/app-store@1.0.0': store,
  appOther: { ping: () => {} },
  'my:pkg/app-other@1.0.0': undefined,
  run,
  'wasi:cli/run@0.3.0': run,
};
exports['my:pkg/app-other@1.0.0'] = exports.appOther;

test('exportNames lists WIT names, sorted, one per instance', () => {
  assert.deepEqual(listExports(exports), [
    'app-hello',
    'my:pkg/app-other@1.0.0',
    'my:pkg/app-other@1.0.0#ping',
    'my:pkg/app-store@1.0.0',
    'my:pkg/app-store@1.0.0#get-all',
    'my:pkg/app-store@1.0.0#put',
    'plain',
    'wasi:cli/run@0.3.0',
    'wasi:cli/run@0.3.0#run',
  ]);
});

test('exportNames with a prefix matches the name or the instance short name', () => {
  assert.deepEqual(listExports(exports, 'app-'), [
    'app-hello',
    'my:pkg/app-other@1.0.0',
    'my:pkg/app-other@1.0.0#ping',
    'my:pkg/app-store@1.0.0',
    'my:pkg/app-store@1.0.0#get-all',
    'my:pkg/app-store@1.0.0#put',
  ]);
  assert.deepEqual(listExports(exports, 'my:pkg/app-s'), ['my:pkg/app-store@1.0.0', 'my:pkg/app-store@1.0.0#get-all', 'my:pkg/app-store@1.0.0#put']);
  assert.deepEqual(listExports(exports, 'nothing'), []);
});

test('exportNames lists an instance jco keys only camel-cased by its WIT name', () => {
  // A composed component's plain-named instance exports: no `ns:pkg/name` key beside the camel one.
  const composed = {
    appHttpGet: { run: () => {} },
    appDnsLookup: { run: () => {}, lookUp: () => {} },
    wasiHelper: { run: () => {} },
    run,
    'wasi:cli/run@0.3.0': run,
  };
  assert.deepEqual(listExports(composed, 'app-'), [
    'app-dns-lookup',
    'app-dns-lookup#look-up',
    'app-dns-lookup#run',
    'app-http-get',
    'app-http-get#run',
  ]);
  for (const name of listExports(composed).filter((n) => n.includes('#'))) {
    assert.equal(typeof findExport(composed, name), 'function', name);
  }
  assert.equal(findExport(composed, 'app-http-get#run'), composed.appHttpGet.run);
});

test('every listed function name is one findExport resolves', () => {
  for (const name of listExports(exports)) {
    if (name.includes('#') || typeof exports[name] === 'function' || name === 'plain' || name === 'app-hello') {
      assert.equal(typeof findExport(exports, name), 'function', name);
    }
  }
});
