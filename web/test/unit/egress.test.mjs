// The egress policy's decisions, with a fake fetch.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { egress, hostMatches } from '../../dist/wasi/egress.js';

const request = (method, url, headers = []) => ({ method, url, headers, body: Promise.resolve(undefined) });

function setup(policy) {
  const log = [];
  const sent = [];
  const fetch = async (url, init) => {
    sent.push({ url, ...init });
    return new Response('{"ok":true}', { status: 200, headers: { 'content-type': 'application/json' } });
  };
  return { handler: egress(policy, { log: (l) => log.push(l), fetch }), log, sent };
}

const denied = (fn, reason) => assert.throws(fn, (e) => e.payload?.tag === 'HTTP-request-denied' && e.message === reason, reason);

test('host patterns: exact, or subdomains of a wildcard but not the bare suffix', () => {
  assert.ok(hostMatches('api.github.com', 'API.GitHub.com'));
  assert.ok(hostMatches('*.example.com', 'a.example.com'));
  assert.ok(hostMatches('*.example.com', 'a.b.example.com'));
  assert.ok(!hostMatches('*.example.com', 'example.com'));
  assert.ok(!hostMatches('*.example.com', 'badexample.com'));
  assert.ok(!hostMatches('example.com', 'a.example.com'));
});

test('everything is denied without a rule, with a note that leaves out the query', () => {
  const { handler, log, sent } = setup({});
  denied(() => handler(request('GET', 'https://api.example.com/v1/x?token=s3cret')), 'no rule allows this host');
  assert.deepEqual(log, ['[codelib] fetch denied: GET https://api.example.com/v1/x (no rule allows this host)']);
  assert.equal(sent.length, 0);
});

test('user info, TRACE/TRACK/CONNECT, plain http and writes under read-only are refused', () => {
  const { handler, log } = setup({ allow: [{ host: 'api.example.com', readOnly: true }] });
  denied(() => handler(request('GET', 'https://u:p@api.example.com/')), 'user info in the URL');
  denied(() => handler(request('TRACE', 'https://api.example.com/')), 'TRACE is never allowed');
  denied(() => handler(request('GET', 'http://api.example.com/')), 'only https is allowed');
  denied(() => handler(request('DELETE', 'https://api.example.com/x')), 'the rule for api.example.com is read-only');
  assert.ok(log.every((l) => !l.includes('u:p')), 'user info logged');
});

test('the first matching rule decides, and its headers replace the request\'s', async () => {
  const { handler, sent } = setup({
    allow: [
      { host: '*.example.com', headers: { Authorization: 'Bearer host-side' } },
      { host: 'api.example.com', readOnly: true },
    ],
  });
  const res = await handler(request('POST', 'https://api.example.com/x', [['authorization', 'Bearer forged'], ['x-mine', '1']]));
  assert.equal(res.status, 200);
  assert.deepEqual(sent[0].headers, [['x-mine', '1'], ['authorization', 'Bearer host-side']]);
  assert.equal(sent[0].redirect, 'error');
  assert.equal(sent[0].credentials, 'omit');
});

test('virtual hosts come first, over http or https, and take a rule\'s headers', async () => {
  const seen = [];
  const { handler, sent } = setup({
    allow: [{ host: 'vault.internal', headers: { authorization: 'Bearer t' } }],
    virtualHosts: { 'vault.internal': (r) => (seen.push(r), { status: 204, headers: [] }) },
  });
  const res = await handler(request('GET', 'http://vault.internal/a?b=1', [['authorization', 'x']]));
  assert.equal(res.status, 204);
  assert.equal(sent.length, 0, 'a virtual host never reaches the network');
  assert.deepEqual(seen[0].headers, [['authorization', 'Bearer t']]);
  assert.equal(seen[0].url, 'http://vault.internal/a?b=1');
});
