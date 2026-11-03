// Wire fixtures for the TypeScript adapter (WI-102).
//
//   node scripts/contracts/export-wire-fixtures.mjs
//
// Writes contracts/wire/fixtures.json from the Rust contracts (the
// `wire_fixtures` example of wrongindexd): canonical samples of every
// request, response, record and enum that crosses the wire, plus the files a
// client reads (endpoint.json, the owner record). `cargo test -p wrongindexd
// --test wire_fixtures` fails when the file is stale; the adapter's test
// checks its own types against a copy. Do not edit the output by hand.
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const json = execFileSync('cargo', ['run', '--quiet', '--locked', '-p', 'wrongindexd', '--example', 'wire_fixtures'], {
  cwd: repo,
  encoding: 'utf8',
  stdio: ['ignore', 'pipe', 'inherit'],
});
const parsed = JSON.parse(json);
const out = path.join(repo, 'contracts', 'wire', 'fixtures.json');
fs.mkdirSync(path.dirname(out), { recursive: true });
fs.writeFileSync(out, `${JSON.stringify(parsed, null, 2)}\n`);
console.log(`${out}: ${Object.keys(parsed.types).length} types, ${parsed.methods.length} methods`);
