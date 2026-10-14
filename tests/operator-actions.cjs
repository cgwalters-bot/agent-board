// The review site's JavaScript JSON Schema semantics, using Ajv 2020.
// Supply Ajv through NODE_PATH; no application dependency is added here.
const assert = require('node:assert/strict');
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
console.log('Shared operator-action schema fixtures passed.');
