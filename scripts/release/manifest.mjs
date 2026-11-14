// Release manifest and third-party licenses (WI-703).
//
//   node scripts/release/manifest.mjs licenses --out <file>
//       THIRD-PARTY-LICENSES.txt for what the shipped binaries contain: every
//       crate the service, CLI and desktop (wrongindex-gpui) link (normal dependencies for
//       x86_64-pc-windows-msvc; build scripts, proc macros and dev
//       dependencies are not shipped), with the license texts found in each
//       crate's source, plus the bundled tree-sitter assets.
//
//   node scripts/release/manifest.mjs manifest --bin <release dir> --bundle <installer dir> --out <file> [--shipped-out <dir>]
//       RELEASE-MANIFEST.json: source commit, the supported index profile
//       (id, manifest hash, WrongStack baseline commit), toolchain, Cargo.lock
//       hash, SHA-256 of every shipped binary and of the installer, bundled
//       assets, and the third-party license summary.
//
//       A binary's hash is the one of the copy the installer ships, unpacked with
//       7-Zip; where the bundler patched it (Tauri's bundle type marker, which
//       only a Tauri app carries; the GPUI desktop has none, so its copy is
//       expected byte-identical), `builtSha256` and `bundlerPatch` say what
//       was built and what changed.
//       Any other difference, or a binary missing from the installer, fails.
//       `--shipped-out` keeps those shipped copies (the release attests them).
//
// Reads only (the installer is unpacked into a temporary directory, removed
// afterwards); runs `cargo metadata --locked`, `git` and 7-Zip.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
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
// PowerShell 7 if present, else Windows PowerShell with its own module
// path (one launched from pwsh inherits pwsh's and cannot load the module).
const authenticode = (p) => {
  if (process.platform !== 'win32') return null;
  const cmd = `(Get-AuthenticodeSignature -LiteralPath '${p.replaceAll("'", "''")}').Status.ToString()`;
  const args = ['-NoProfile', '-NonInteractive', '-Command', cmd];
  for (const [exe, env] of [
    ['pwsh', process.env],
    ['powershell', Object.fromEntries(Object.entries(process.env).filter(([k]) => k.toUpperCase() !== 'PSMODULEPATH'))],
  ]) {
    try {
      const out = execFileSync(exe, args, { encoding: 'utf8', env, stdio: ['ignore', 'pipe', 'ignore'] }).trim();
      if (out) return out;
    } catch {}
  }
  return null;
};
const SHIPPED = ['wrongindexd', 'wrongindex-cli', 'wrongindex-gpui'];
const BINARIES = ['wrongindexd.exe', 'wrongindex.exe', 'wrongindex-gpui.exe'];

// The installer's files, unpacked by 7-Zip (it reads NSIS installers) into a
// new temporary directory the caller removes.
function extract(installer) {
  const candidates = [path.join(process.env.ProgramFiles ?? 'C:\\Program Files', '7-Zip', '7z.exe'), '7z'];
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'wrongindex-manifest-'));
  for (const exe of candidates) {
    try {
      execFileSync(exe, ['x', installer, `-o${dir}`, '-y'], { stdio: ['ignore', 'ignore', 'pipe'] });
      return dir;
    } catch (e) {
      if (e.code !== 'ENOENT') {
        fs.rmSync(dir, { recursive: true, force: true });
        throw new Error(`unpacking ${installer} with ${exe}: ${e.message}`);
      }
    }
  }
  fs.rmSync(dir, { recursive: true, force: true });
  throw new Error('7-Zip (7z) is needed to read the binaries the installer ships');
}

// The file named `name` (any letter case) under `dir`, or null.
function findFile(dir, name) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) {
      const found = findFile(p, name);
      if (found) return found;
    } else if (e.name.toLowerCase() === name.toLowerCase()) {
      return p;
    }
  }
  return null;
}

// Tauri's bundler writes the bundle type into a Tauri app binary it packs
// (`__TAURI_BUNDLE_TYPE_VAR_UNK` becomes `..._NSS` for NSIS), so such a
// shipped copy differs from the build in those three bytes. The GPUI desktop
// carries no marker (the bundler only warns), so none is expected now. What the patch was, when
// that is the only difference; null otherwise.
const MARKER = Buffer.from('__TAURI_BUNDLE_TYPE_VAR_');
function patchOnly(built, shipped) {
  if (built.length !== shipped.length) return null;
  const patches = [];
  let from = 0;
  const allowed = [];
  for (let at = built.indexOf(MARKER, from); at >= 0; at = built.indexOf(MARKER, from)) {
    const tag = at + MARKER.length;
    allowed.push([tag, tag + 3]);
    from = tag;
  }
  for (let i = 0; i < built.length; i++) {
    if (built[i] === shipped[i]) continue;
    const span = allowed.find(([s, e]) => i >= s && i < e);
    if (!span) return null;
    const [s, e] = span;
    const text = `${MARKER}${built.subarray(s, e)} -> ${MARKER}${shipped.subarray(s, e)} at 0x${(s - MARKER.length).toString(16)}`;
    if (!patches.includes(text)) patches.push(text);
    i = e - 1;
  }
  return patches.length ? `Tauri bundle type marker: ${patches.join('; ')}` : null;
}

// The DLLs a PE file imports (its import directory), lower-cased.
function peImports(file) {
  const b = fs.readFileSync(file);
  const pe = b.readUInt32LE(0x3c);
  if (b.toString('latin1', pe, pe + 4) !== 'PE\0\0') throw new Error(`${file}: not a PE file`);
  const coff = pe + 4;
  const sections = b.readUInt16LE(coff + 2);
  const optSize = b.readUInt16LE(coff + 16);
  const opt = coff + 20;
  const dirs = opt + (b.readUInt16LE(opt) === 0x20b ? 112 : 96);
  const importRva = b.readUInt32LE(dirs + 8);
  const table = opt + optSize;
  const offset = (rva) => {
    for (let i = 0; i < sections; i++) {
      const s = table + i * 40;
      const va = b.readUInt32LE(s + 12);
      const size = Math.max(b.readUInt32LE(s + 8), b.readUInt32LE(s + 16));
      if (rva >= va && rva < va + size) return rva - va + b.readUInt32LE(s + 20);
    }
    throw new Error(`${file}: address ${rva} is outside every section`);
  };
  const names = new Set();
  if (importRva) {
    for (let d = offset(importRva); b.readUInt32LE(d + 12) !== 0; d += 20) {
      const at = offset(b.readUInt32LE(d + 12));
      names.add(b.toString('latin1', at, b.indexOf(0, at)).toLowerCase());
    }
  }
  return [...names].sort();
}
// The Visual C++ runtime is not part of Windows: a clean machine has no
// vcruntime140.dll, and a binary importing it does not start (M05 found the
// service and the CLI failing that way). The C runtime is linked statically
// (.cargo/config.toml), and a binary that needs the redistributable is not
// shipped.
const VC_REDIST = /^(vcruntime|msvcp|vccorlib|concrt|vcomp|ucrtbased)/;

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
    `Rust crates linked into wrongindexd, wrongindex and wrongindex-gpui: ${crates.length}.`,
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
  const selectedInstaller = opt('--installer');
  const out = opt('--out');
  // Where to leave the binaries as the installer ships them (attested).
  const shippedOut = opt('--shipped-out');
  if (!bin || !out) throw new Error('--bin <release dir> --out <file> [--bundle <installer dir> [--shipped-out <dir>]]');
  const crates = shippedCrates();
  const histogram = {};
  for (const c of crates) histogram[c.license ?? '(none)'] = (histogram[c.license ?? '(none)'] ?? 0) + 1;
  const profileDir = path.join(repo, 'contracts', 'profiles', 'wrongstack-index-v5-baseline');
  const profile = JSON.parse(fs.readFileSync(path.join(profileDir, 'manifest.json'), 'utf8'));
  const binaries = {};
  for (const n of BINARIES) {
    const p = path.join(bin, n);
    if (fs.existsSync(p)) binaries[n] = { sha256: sha256(p), bytes: fs.statSync(p).size, authenticode: authenticode(p), imports: peImports(p) };
  }
  const needsRedist = Object.entries(binaries).flatMap(([n, b]) => b.imports.filter((d) => VC_REDIST.test(d)).map((d) => `${n} (${d})`));
  const installers = {};
  const notShipped = [];
  if (bundle && fs.existsSync(bundle)) {
    if (selectedInstaller && !fs.existsSync(path.join(bundle, selectedInstaller))) throw new Error('Selected installer is missing from the bundle directory');
    for (const n of fs.readdirSync(bundle).filter((f) => f.endsWith('.exe') && (!selectedInstaller || f === selectedInstaller))) {
      const p = path.join(bundle, n);
      installers[n] = { sha256: sha256(p), bytes: fs.statSync(p).size, authenticode: authenticode(p) };
      // Each binary as the installer ships it, which is what a user can
      // check: a bundler patch would show here (see `patchOnly`).
      const dir = extract(p);
      try {
        for (const [name, b] of Object.entries(binaries)) {
          const file = findFile(dir, name);
          if (!file) {
            notShipped.push(`${name} (not in ${n})`);
            continue;
          }
          if (shippedOut) {
            fs.mkdirSync(shippedOut, { recursive: true });
            fs.copyFileSync(file, path.join(shippedOut, name));
          }
          const got = sha256(file);
          if (got === b.sha256) continue;
          const why = patchOnly(fs.readFileSync(path.join(bin, name)), fs.readFileSync(file));
          if (!why) {
            notShipped.push(`${name} (the copy in ${n} differs from the build beyond the bundler's marker)`);
            continue;
          }
          binaries[name] = { ...b, sha256: got, bytes: fs.statSync(file).size, builtSha256: b.sha256, bundlerPatch: why };
        }
      } finally {
        fs.rmSync(dir, { recursive: true, force: true });
      }
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
  const absent = BINARIES.filter((n) => !binaries[n]);
  if (buildInfo && !buildInfo.sqlite?.fts5Trigram) absent.push('SQLite FTS5 trigram tokenizer');
  if (notShipped.length) {
    console.error(`not shipped as built: ${notShipped.join(', ')}`);
    process.exit(1);
  }
  if (absent.length || (bundle && !Object.keys(installers).length)) {
    console.error(`missing: ${[...absent, ...(bundle && !Object.keys(installers).length ? ['installer'] : [])].join(', ')}`);
    process.exit(1);
  }
  if (needsRedist.length) {
    console.error(`needs the Visual C++ runtime, which a clean Windows does not have: ${needsRedist.join(', ')}`);
    process.exit(1);
  }
} else {
  console.error('usage: manifest.mjs licenses --out <file> | manifest --bin <dir> --bundle <dir> --out <file>');
  process.exit(2);
}
