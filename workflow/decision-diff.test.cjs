const assert = require('node:assert/strict');
const {spawnSync} = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const {test} = require('node:test');

const script = path.join(__dirname, 'decision-diff.cjs');

test('timeouts and overflow kill a preview that ignores SIGTERM', () => {
  for (const flood of [false, true]) {
    // An outer hard deadline keeps a regression from hanging the test suite.
    const probe = spawnSync(process.execPath, ['-e', `
      const {run} = require(${JSON.stringify(script)});
      const child = "process.on('SIGTERM', () => {}); " +
        ${JSON.stringify(flood ? "setInterval(() => process.stdout.write('x'.repeat(8192)), 1)" : 'setInterval(() => {}, 10)')};
      const result = run(process.execPath, ['-e', child], {timeout: 500, maxBuffer: 1024});
      console.log(JSON.stringify({code: result.error?.code, signal: result.signal}));
    `], {encoding: 'utf8', timeout: 5000, killSignal: 'SIGKILL'});
    assert.equal(probe.status, 0, probe.stderr);
    assert.deepEqual(JSON.parse(probe.stdout), {
      code: flood ? 'ENOBUFS' : 'ETIMEDOUT', signal: 'SIGKILL',
    });
  }
});

function reportProbe({noisyBuild = false, buildFails = false} = {}) {
  const output = [];
  const calls = [];
  const longLines = ('é'.repeat(2048) + '\n').repeat(20);
  const source = fs.readFileSync(script, 'utf8')
    .replace(/^main\(process.argv.slice\(2\)\);$/m, '');
  const stream = {write: (text) => output.push(text)};
  const context = {
    require: (name) => name === 'node:child_process' ? {
      spawnSync: (command, args, options) => {
        calls.push({command, options});
        if (command === 'git') return {status: 0, stdout: 'revision\n'};
        if (command === 'cargo') return {
          status: buildFails ? 1 : 0, stdout: noisyBuild ? longLines : '', stderr: noisyBuild ? longLines : '',
        };
        if (command === 'diff') return {status: 1, stdout: longLines.repeat(3), stderr: longLines};
        return {status: 1, stdout: 'summary:' + longLines, stderr: longLines};
      },
    } : require(name),
    module: {}, __filename: script, Buffer,
    process: {stdout: stream, stderr: stream, env: {}, exit: (code) => { throw new Error(`exit ${code}`); }},
  };
  try {
    vm.runInNewContext(source + '\nmain([".", "."]);', context);
  } catch (error) {
    if (!buildFails || error.message !== 'exit 1') throw error;
  }
  return {output: output.join(''), calls};
}

test('long-line failures and noisy builds share one report budget', () => {
  for (const options of [{}, {noisyBuild: true}, {noisyBuild: true, buildFails: true}]) {
    const {output, calls} = reportProbe(options);
    assert.ok(Buffer.byteLength(output) <= 24 * 1024, Buffer.byteLength(output));
    assert.ok(output.includes(options.buildFails ? 'cargo build failed' : 'head: exit 1'));
    for (const {options: childOptions} of calls) {
      assert.equal(childOptions.killSignal, 'SIGKILL');
      assert.ok(childOptions.maxBuffer > 0);
      assert.ok(childOptions.timeout > 0);
      assert.notEqual(childOptions.stdio?.[1], 'inherit');
      assert.notEqual(childOptions.stdio?.[2], 'inherit');
    }
  }
});
