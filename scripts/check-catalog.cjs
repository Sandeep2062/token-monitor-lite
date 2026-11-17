// Remote test tooling only. These two reviewed pure-data reference modules
// require no Node built-ins and do not initialize the reference application.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const clients = require('../reference/clientCatalog.js');
const providers = require('../reference/providers.js');
const native = JSON.parse(fs.readFileSync('assets/catalog.json', 'utf8'));

// Clients this application tracks that upstream does not. They are listed in
// README.md under "Upstream parity"; this list is pinned so a client can never
// be added to the catalog without saying so here, while every upstream row
// stays exactly what upstream declares.
const EXTENSION_CLIENTS = [
  'kimchi',
  'mux',
  'junie',
  'augment',
  'gjc',
  'codebuff',
  'freebuff',
  'opencodereview',
  'trae',
  'bb',
  'roocode',
];

const normalizedClients = native.clients.map(row => ({
  defaultTracked: true, forkOnly: false, ...row
}));
const upstreamIds = new Set(clients.CLIENT_IDS);
for (const id of EXTENSION_CLIENTS) {
  assert.ok(!upstreamIds.has(id), `${id} is an extension client, not an upstream one`);
}
const upstreamRows = normalizedClients.filter(row => !EXTENSION_CLIENTS.includes(row.id));
assert.deepStrictEqual(upstreamRows, clients.CLIENT_CATALOG);
const presentExtensions = normalizedClients
  .filter(row => EXTENSION_CLIENTS.includes(row.id))
  .map(row => row.id)
  .sort();
assert.deepStrictEqual(presentExtensions, [...EXTENSION_CLIENTS].sort());
assert.deepStrictEqual(native.providers, providers.LIMIT_PROVIDER_CATALOG);
const expectedMapping = Object.fromEntries(clients.CLIENT_IDS
  .map(id => [id, providers.limitProviderForClient(id)])
  .filter(([id, provider]) => provider !== null && id !== provider));
assert.deepStrictEqual(native.providerByClient, expectedMapping);
console.log(`Exact catalog differential passed: ${upstreamRows.length} upstream clients, ${presentExtensions.length} documented extensions, ${native.providers.length} providers, all rows/order/flags/labels/mapping.`);
