// Release manifest and third-party licenses (WI-703).
//
//   node scripts/release/manifest.mjs licenses --out <file>
//       THIRD-PARTY-LICENSES.txt for what the shipped binaries contain: every
//       crate the service, CLI and desktop link (normal dependencies for
//       x86_64-pc-windows-msvc; build scripts, proc macros and dev
//       dependencies are not shipped), with the license texts found in each
//       crate's source, plus the bundled tree-sitter assets.
//
//   node scripts/release/manifest.mjs manifest --bin <release dir> --bundle <installer dir> --out <file>
//       RELEASE-MANIFEST.json: source commit, the supported index profile
//       (id, manifest hash, WrongStack baseline commit), toolchain, Cargo.lock
//       hash, SHA-256 of every shipped binary and of the installer, bundled
//       assets, and the third-party license summary.
//
// Reads only; runs `cargo metadata --locked` and `git`.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const [mode, ...rest] = process.argv.slice(2);
const opt = (n) => {
  const i = rest.indexOf(n);
  return i >= 0 ? rest[i + 1] : undefined;
};
const sha256 = (p) => createHash('sha256').update(fs.readFileSync(p)).digest('hex');
const sh = (cmd, args) => execFileSync(cmd, args, { cwd: repo, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 }).trim();
const tryRun = (cmd, args) => {
  try {
    return sh(cmd, args);
  } catch {
    return null;
  }
};
// Authenticode status as Windows reports it (Valid, NotSigned, ...): the
// manifest records what was shipped, not what was intended.
const authenticode = (p) =>
  process.platform === 'win32'
    ? tryRun('powershell', [
        '-NoProfile',
        '-NonInteractive',
        '-Command',
        `(Get-AuthenticodeSignature -LiteralPath '${p.replaceAll("'", "''")}').Status.ToString()`,
      ])
    : null;
const SHIPPED = ['wrongindexd', 'wrongindex-cli', 'wrongindex-desktop'];

function shippedCrates() {
  const meta = JSON.parse(
    sh('cargo', ['metadata', '--format-version', '1', '--locked', '--filter-platform', 'x86_64-pc-windows-msvc']),
  );
  const byId = new Map(meta.packages.map((p) => [p.id, p]));
  const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
  const isProcMacro = (p) => p.targets.some((t) => t.kind.includes('proc-macro'));
  const seen = new Set();
  const stack = meta.packages.filter((p) => SHIPPED.includes(p.name) && meta.workspace_members.includes(p.id)).map((p) => p.id);
  while (stack.length) {
    const id = stack.pop();
    if (seen.has(id)) continue;
    seen.add(id);
    for (const d of nodes.get(id)?.deps ?? []) {
      const normal = d.dep_kinds.some((k) => k.kind === null);
      const dep = byId.get(d.pkg);
      if (normal && dep && !isProcMacro(dep)) stack.push(d.pkg);
    }
  }
  return [...seen]
    .map((id) => byId.get(id))
    .filter((p) => !meta.workspace_members.includes(p.id))
    .sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
}

function licenseTexts(pkg) {
  const dir = path.dirname(pkg.manifest_path);
  const files = fs
    .readdirSync(dir)
    .filter((f) => /^(licen[cs]e|copying|notice|copyright)/i.test(f) && fs.statSync(path.join(dir, f)).isFile())
    .sort();
  if (pkg.license_file && !files.includes(pkg.license_file)) files.push(pkg.license_file);
  return files.map((f) => ({ file: f, text: fs.readFileSync(path.join(dir, f), 'utf8') }));
}

const assetLicensesDir = path.join(repo, 'crates', 'wrongindex-engine', 'tree-sitter', 'LICENSES');
const assetLicenses = () => JSON.parse(fs.readFileSync(path.join(assetLicensesDir, 'licenses.json'), 'utf8'));
const treeSitter = () => {
  const p = path.join(repo, 'crates', 'wrongindex-engine', 'tree-sitter', 'provenance.json');
  return JSON.parse(fs.readFileSync(p, 'utf8'));
};

if (mode === 'licenses') {
  const out = opt('--out');
  if (!out) throw new Error('--out <file>');
  const crates = shippedCrates();
  const ts = treeSitter();
  const assets = assetLicenses();
  const lines = [
    'WrongIndex third-party licenses',
    '================================',
    '',
    'WrongIndex itself: MIT License, Copyright (c) 2026 ECOSTACK TECHNOLOGY OÜ (see LICENSE).',
    '',
    `Rust crates linked into wrongindexd, wrongindex and wrongindex-desktop: ${crates.length}.`,
    '',
    `Bundled JavaScript and WebAssembly (the tree-sitter route, from WrongStack ${ts.wrongstackCommit}):`,
    ...assets.components.map((c) => `  ${c.component} ${c.version} (${c.license})`),
    '',
  ];
  const tampered = [];
  for (const c of assets.components) {
    const text = fs.readFileSync(path.join(assetLicensesDir, c.file), 'utf8');
    if (createHash('sha256').update(text).digest('hex') !== c.sha256) tampered.push(c.file);
    lines.push('-'.repeat(78), `${c.component} ${c.version} (bundled asset)`, `License: ${c.license}`, `Source: ${c.source}`, '');
    lines.push(text.trimEnd(), '');
  }
  const missing = [];
  for (const c of crates) {
    lines.push('-'.repeat(78), `${c.name} ${c.version}`, `License: ${c.license ?? '(none declared)'}`);
    if (c.repository) lines.push(`Source: ${c.repository}`);
    if (c.authors?.length) lines.push(`Authors: ${c.authors.join(', ')}`);
    const texts = licenseTexts(c);
    if (!texts.length) {
      missing.push(`${c.name} ${c.version}`);
      lines.push('(no license file in the published crate; the declared license applies)');
    }
    for (const t of texts) lines.push('', `--- ${t.file} ---`, t.text.trimEnd());
    lines.push('');
  }
  fs.mkdirSync(path.dirname(path.resolve(out)), { recursive: true });
  fs.writeFileSync(out, `${lines.join('\n')}\n`);
  // A crate that declares no license at all cannot be shipped; a bundled
  // text that no longer matches its recorded hash was edited by hand.
  const undeclared = crates.filter((c) => !c.license && !c.license_file).map((c) => `${c.name} ${c.version}`);
  console.log(JSON.stringify({ crates: crates.length, assets: assets.components.length, withoutLicenseFile: missing, undeclared, tampered }));
  if (undeclared.length || tampered.length) process.exit(1);
} else if (mode === 'manifest') {
  const bin = opt('--bin');
  const bundle = opt('--bundle');
  const out = opt('--out');
  if (!bin || !out) throw new Error('--bin <release dir> --out <file> [--bundle <installer dir>]');
  const crates = shippedCrates();
  const histogram = {};
  for (const c of crates) histogram[c.license ?? '(none)'] = (histogram[c.license ?? '(none)'] ?? 0) + 1;
  const profileDir = path.join(repo, 'contracts', 'profiles', 'wrongstack-index-v5-baseline');
  const profile = JSON.parse(fs.readFileSync(path.join(profileDir, 'manifest.json'), 'utf8'));
  const binaries = {};
  for (const n of ['wrongindexd.exe', 'wrongindex.exe', 'wrongindex-desktop.exe']) {
    const p = path.join(bin, n);
    if (fs.existsSync(p)) binaries[n] = { sha256: sha256(p), bytes: fs.statSync(p).size, authenticode: authenticode(p) };
  }
  const installers = {};
  if (bundle && fs.existsSync(bundle)) {
    for (const n of fs.readdirSync(bundle).filter((f) => f.endsWith('.exe'))) {
      const p = path.join(bundle, n);
      installers[n] = { sha256: sha256(p), bytes: fs.statSync(p).size, authenticode: authenticode(p) };
    }
  }
  const ts = treeSitter();
  // The SQLite the shipped service runs (probed by the binary itself): the
  // profile needs FTS5 with the trigram tokenizer.
  const buildInfo = binaries['wrongindexd.exe'] ? JSON.parse(execFileSync(path.join(bin, 'wrongindexd.exe'), ['--build-info'], { encoding: 'utf8' })) : null;
  const manifest = {
    schema: 'wrongindex.release-manifest/1',
    source: {
      repository: 'https://github.com/WrongStack/wrongindex',
      commit: sh('git', ['rev-parse', 'HEAD']),
      dirtyWorktree: sh('git', ['status', '--porcelain']).length > 0,
      version: JSON.parse(sh('cargo', ['metadata', '--format-version', '1', '--locked', '--no-deps'])).packages.find((p) => p.name === 'wrongindexd')?.version,
    },
    supportedProfile: {
      id: profile.id,
      manifestSha256: sha256(path.join(profileDir, 'manifest.json')),
      schemaSha256: sha256(path.join(profileDir, 'schema.sql')),
      wrongstackCommit: profile.provenance?.wrongstackCommit,
    },
    toolchain: { rustc: sh('rustc', ['-V']), cargo: sh('cargo', ['-V']), tauriCli: tryRun('cargo', ['tauri', '--version']), node: process.version },
    build: process.env.GITHUB_ACTIONS
      ? {
          system: 'github-actions',
          repository: process.env.GITHUB_REPOSITORY,
          workflow: process.env.GITHUB_WORKFLOW,
          ref: process.env.GITHUB_REF,
          runId: process.env.GITHUB_RUN_ID,
          runAttempt: process.env.GITHUB_RUN_ATTEMPT,
        }
      : { system: 'local' },
    cargoLockSha256: sha256(path.join(repo, 'Cargo.lock')),
    sqlite: buildInfo?.sqlite ?? null,
    sqliteRequired: profile.sqliteRequirements ?? null,
    binaries,
    installers,
    bundledAssets: {
      treeSitter: {
        wrongstackCommit: ts.wrongstackCommit,
        provenanceSha256: sha256(path.join(repo, 'crates', 'wrongindex-engine', 'tree-sitter', 'provenance.json')),
        licenses: Object.fromEntries(assetLicenses().components.map((c) => [c.component, `${c.version} ${c.license}`])),
        licensesSha256: sha256(path.join(assetLicensesDir, 'licenses.json')),
      },
    },
    thirdParty: { crates: crates.length, licenses: histogram, file: 'THIRD-PARTY-LICENSES.txt' },
  };
  fs.writeFileSync(out, `${JSON.stringify(manifest, null, 2)}\n`);
  console.log(out);
  const absent = ['wrongindexd.exe', 'wrongindex.exe', 'wrongindex-desktop.exe'].filter((n) => !binaries[n]);
  if (buildInfo && !buildInfo.sqlite?.fts5Trigram) absent.push('SQLite FTS5 trigram tokenizer');
  if (absent.length || (bundle && !Object.keys(installers).length)) {
    console.error(`missing: ${[...absent, ...(bundle && !Object.keys(installers).length ? ['installer'] : [])].join(', ')}`);
    process.exit(1);
  }
} else {
  console.error('usage: manifest.mjs licenses --out <file> | manifest --bin <dir> --bundle <dir> --out <file>');
  process.exit(2);
}
