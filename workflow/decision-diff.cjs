// The decision diff (docs/setup.md#compare-two-revisions): the recorded pass
// previewed by two checkouts' binaries from the same configuration and
// recording, so the only input that differs is the code. Standard library
// only, as the review sandbox and an implement run have node, cargo and diff
// but nothing installed from npm.
//
//   node workflow/decision-diff.cjs BASE HEAD [CONFIG [RECORDING]]
//
// Builds agent-board in BASE and HEAD, previews the replay with each, and
// prints both summary lines and the unified diff of the two previews. Exits 0
// when both previews ran, whatever they decided: a head whose replay fails,
// hangs or floods is reported with its exit status, not hidden. Exits 1 when
// a build failed or the arguments are wrong.
const {spawnSync} = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const DEFAULT_CONFIG = 'fixtures/pass/config.json';
const DEFAULT_RECORDING = 'fixtures/pass/board-2026-10-23.json';
// The offline rehearsal's caps (docs/setup.md): what the replay fixture's test expects.
const PASS_CAPS = ['--max-dispatch', '1', '--max-writes', '3', '--max-status-changes', '5'];
// CI allows the rust job 20 minutes; a replay reads one file and sends nothing.
const BUILD_TIMEOUT_MS = 20 * 60 * 1000;
const PASS_TIMEOUT_MS = 60 * 1000;
// A preview's output is read up to this; past it, the preview is cut.
const MAX_OUTPUT_BYTES = 256 * 1024;
// What this prints goes whole into a review comment, which GitHub refuses
// past 65536 characters: a head that prints without end, or one line
// without end, is cut here, not pasted whole.
const MAX_DIFF_LINES = 400;
const MAX_DIFF_BYTES = 16 * 1024;
const MAX_LINE_BYTES = 4 * 1024;
const FAILURE_TAIL_LINES = 20;
const MAX_REPORT_BYTES = 24 * 1024;
const MAX_DIAGNOSTIC_BYTES = 4 * 1024;
const REPORT_CUT = '\n... decision diff report truncated\n';
let reportBytes = 0;

// One budget for stdout and stderr together, including build diagnostics and
// failure paths. Leave most of the comment's 65,000 characters for findings.
function write(text, stream = process.stdout) {
  const remaining = MAX_REPORT_BYTES - Buffer.byteLength(REPORT_CUT) - reportBytes;
  if (remaining < 0) return;
  const shown = cut(text, remaining);
  stream.write(shown);
  reportBytes += Buffer.byteLength(shown);
  if (shown !== text) {
    stream.write(REPORT_CUT);
    reportBytes = MAX_REPORT_BYTES;
  }
}

function fail(message) {
  write(`decision-diff: ${message}\n`, process.stderr);
  process.exit(1);
}

// Neither the head's build (its build scripts and proc macros run here) nor
// a preview gets a forge token: a preview must not reach the forge even by
// mistake, and the review sandbox runs the head's code only as data.
function withoutTokens(env) {
  const clean = {...env};
  for (const name of ['GH_TOKEN', 'GITHUB_TOKEN', 'DISPATCH_TOKEN']) delete clean[name];
  return clean;
}

function cut(text, bytes) {
  if (Buffer.byteLength(text) <= bytes) return text;
  const marker = '... cut';
  if (bytes < marker.length) return marker.slice(0, bytes);
  return Buffer.from(text).subarray(0, bytes - marker.length).toString().replace(/\uFFFD$/, '') + marker;
}

function run(command, args, options = {}) {
  // spawnSync waits after sending its timeout/overflow signal. SIGTERM can
  // be ignored by the head; SIGKILL cannot, so the deadline is enforceable.
  return spawnSync(command, args, {
    encoding: 'utf8', timeout: PASS_TIMEOUT_MS, maxBuffer: MAX_OUTPUT_BYTES,
    ...options, killSignal: 'SIGKILL',
  });
}

function revision(dir) {
  const git = run('git', ['-C', dir, 'describe', '--always', '--dirty']);
  return git.status === 0 ? git.stdout.trim() : 'no git revision';
}

function build(dir) {
  const cargo = run('cargo', ['build', '--locked', '-q', '-p', 'agent-board'], {
    cwd: dir, env: withoutTokens(process.env), timeout: BUILD_TIMEOUT_MS,
  });
  const diagnostics = `${cargo.stdout ?? ''}${cargo.stderr ?? ''}`;
  if (diagnostics) write(`cargo build in ${dir}:\n${cut(diagnostics, MAX_DIAGNOSTIC_BYTES)}\n`);
  if (cargo.status !== 0) fail(`cargo build failed in ${dir} (${cargo.error?.code ?? cargo.signal ?? cargo.status})`);
  return path.join(dir, 'target', 'debug', 'agent-board');
}

function preview(binary, dir, config, recording) {
  const pass = run(binary, ['pass', '--config', config, '--replay', recording, ...PASS_CAPS], {
    cwd: dir, env: withoutTokens(process.env), encoding: 'utf8', timeout: PASS_TIMEOUT_MS, maxBuffer: MAX_OUTPUT_BYTES,
  });
  // A timeout (ETIMEDOUT) or an overflow (ENOBUFS) is the preview's own
  // outcome; a binary that cannot run at all is this script's failure.
  if (pass.error && !['ETIMEDOUT', 'ENOBUFS'].includes(pass.error.code)) fail(`${binary}: ${pass.error.message}`);
  const output = `${pass.stdout ?? ''}${pass.stderr ?? ''}`;
  const exit = pass.error?.code ?? pass.signal ?? pass.status;
  const summary = output.split('\n').filter((line) => line.startsWith('summary:')).pop() ?? 'no summary line';
  return {exit, output, summary};
}

function report(name, {exit, summary, output}) {
  write(`${name}: exit ${exit}, ${cut(summary, MAX_LINE_BYTES)}\n`);
  // A failure's reason is in its last lines, which an identical failure on
  // both sides would otherwise hide behind "no change".
  if (exit !== 0) {
    const tail = output.trim() ? output.trimEnd().split('\n').slice(-FAILURE_TAIL_LINES) : ['(no output)'];
    write(`${name} output, last ${tail.length} line(s):\n${cut(tail.map((line) => `  ${cut(line, MAX_LINE_BYTES)}`).join('\n'), MAX_DIAGNOSTIC_BYTES)}\n`);
  }
}

function main(args) {
  if (args.length < 2 || args.length > 4) fail('usage: node workflow/decision-diff.cjs BASE HEAD [CONFIG [RECORDING]]');
  const [base, head] = args.slice(0, 2).map((dir) => path.resolve(dir));
  const config = path.resolve(args[2] ?? path.join(base, DEFAULT_CONFIG));
  const recording = path.resolve(args[3] ?? path.join(base, DEFAULT_RECORDING));
  // Which script ran, so a review shows it was the base's copy.
  write(`decision diff: base ${revision(base)}, head ${revision(head)}, config ${config}, recording ${recording}, script ${fs.realpathSync(__filename)}\n`);
  const previews = [];
  // Each side is reported as it completes: a head that fails to build
  // still leaves the base's summary on record.
  for (const [name, dir] of [['base', base], ['head', head]]) {
    const result = preview(build(dir), dir, config, recording);
    report(name, result);
    previews.push(result);
  }
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'decision-diff-'));
  const files = ['base', 'head'].map((name, i) => {
    const file = path.join(dir, name);
    fs.writeFileSync(file, previews[i].output);
    return file;
  });
  const diff = run('diff', ['-u', '--label', 'base', '--label', 'head', ...files], {
    maxBuffer: 2 * MAX_OUTPUT_BYTES,
  });
  fs.rmSync(dir, {recursive: true});
  if (diff.stderr) write(cut(diff.stderr, MAX_DIAGNOSTIC_BYTES), process.stderr);
  if (diff.error) fail(`diff: ${diff.error.message}`);
  if (diff.status === 0) {
    write(`no change in ${previews[0].output.split('\n').length - 1} line(s)\n`);
    return;
  }
  if (diff.status !== 1) fail(`diff exited ${diff.status}`);
  const lines = diff.stdout.replace(/\n$/, '').split('\n');
  const shown = cut(lines.slice(0, MAX_DIFF_LINES).join('\n'), MAX_DIFF_BYTES);
  const rest = lines.length - MAX_DIFF_LINES;
  write(rest > 0 ? `${shown}\n... ${rest} more line(s)\n` : `${shown}\n`);
}

if (require.main === module) main(process.argv.slice(2));
module.exports = {run};
