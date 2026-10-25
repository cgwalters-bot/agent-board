// The review site's JavaScript JSON Schema semantics, using Ajv 2020.
// Supply Ajv through NODE_PATH; no application dependency is added here.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const Ajv = require('ajv/dist/2020');

const schema = JSON.parse(fs.readFileSync('docs/operator-actions.schema.json', 'utf8'));
const fixture = JSON.parse(fs.readFileSync('fixtures/operator-actions.json', 'utf8'));
const validate = new Ajv({strict: true}).compile(schema);
assert.equal(validate(fixture.valid), true, JSON.stringify(validate.errors));
for (const [key, expected] of [['valid_actions', true], ['invalid_actions', false]]) {
  for (const action of fixture[key]) {
    assert.equal(validate({...fixture.valid, actions: [action]}), expected,
      `${JSON.stringify(action)}: ${JSON.stringify(validate.errors)}`);
  }
}
// Repository membership is configuration, not a restriction of the schema.
for (const action of fixture.scope_rejections) {
  assert.equal(validate({...fixture.valid, actions: [action]}), true);
}
// Ruleset digests: writable fields, keys sorted, compact JSON, SHA-256 (docs/operator.md).
const WRITABLE = ['name', 'target', 'enforcement', 'bypass_actors', 'conditions', 'rules'];
// Written out by hand: JSON.stringify would put integer-like keys ("2" before "10") first.
const canonical = (v) => Array.isArray(v) ? `[${v.map(canonical).join(',')}]` : v && typeof v === 'object'
  ? `{${Object.keys(v).sort().map((k) => `${JSON.stringify(k)}:${canonical(v[k])}`).join(',')}}` : JSON.stringify(v);
for (const {ruleset, sha256} of fixture.ruleset_digests) {
  const body = canonical(Object.fromEntries(Object.entries(ruleset).filter(([k]) => WRITABLE.includes(k))));
  assert.equal(crypto.createHash('sha256').update(body, 'utf8').digest('hex'), sha256, body);
}
console.log('Shared operator-action schema fixtures passed.');
