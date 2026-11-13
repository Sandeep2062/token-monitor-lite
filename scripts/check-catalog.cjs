// Remote test tooling only. These two reviewed pure-data reference modules
// require no Node built-ins and do not initialize the reference application.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const clients = require('../reference/clientCatalog.js');
const providers = require('../reference/providers.js');
const native = JSON.parse(fs.readFileSync('assets/catalog.json', 'utf8'));
const normalizedClients = native.clients.map(row => ({
  defaultTracked: true, forkOnly: false, ...row
}));
assert.deepStrictEqual(normalizedClients, clients.CLIENT_CATALOG);
assert.deepStrictEqual(native.providers, providers.LIMIT_PROVIDER_CATALOG);
const expectedMapping = Object.fromEntries(clients.CLIENT_IDS
  .map(id => [id, providers.limitProviderForClient(id)])
  .filter(([id, provider]) => provider !== null && id !== provider));
assert.deepStrictEqual(native.providerByClient, expectedMapping);
console.log(`Exact catalog differential passed: ${native.clients.length} clients, ${native.providers.length} providers, all rows/order/flags/labels/mapping.`);
