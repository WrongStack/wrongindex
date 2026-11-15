// A reproducible .tar.gz of one directory (WI-M8-06): the same files give
// the same bytes on any machine. Entries are sorted by name; owner and group
// are 0 with no names; every time stamp is `--mtime` (the commit time); a
// mode is 755 for a directory or an executable file, else 644. Only regular
// files and directories are packed (anything else is refused). gzip carries
// no name or time, and its OS byte is "unknown" on every system.
//
//   bun scripts/release/tar.mjs --dir <parent> --name <directory> --mtime <unix seconds> --out <file.tar.gz>
import * as fs from 'node:fs';
import * as path from 'node:path';
import { gzipSync } from 'node:zlib';

const args = process.argv.slice(2);
const opt = (n) => {
  const i = args.indexOf(n);
  return i >= 0 ? args[i + 1] : undefined;
};
const [parent, name, mtimeText, out] = [opt('--dir'), opt('--name'), opt('--mtime'), opt('--out')];
if (!parent || !name || !mtimeText || !out) throw new Error('--dir <parent> --name <directory> --mtime <unix seconds> --out <file.tar.gz>');
const mtime = Number(mtimeText);
if (!Number.isInteger(mtime) || mtime < 0) throw new Error(`--mtime ${mtimeText}: not a time`);

const BLOCK = 512;
function octal(value, width) {
  const s = value.toString(8);
  if (s.length > width - 1) throw new Error(`${value} does not fit a ${width}-byte field`);
  return `${s.padStart(width - 1, '0')}\0`;
}
// One ustar header; names are kept under 100 bytes (no prefix field).
function header(entry, mode, size, type) {
  const h = Buffer.alloc(BLOCK);
  const nameBytes = Buffer.from(entry, 'utf8');
  if (nameBytes.length >= 100) throw new Error(`${entry}: name too long for this archive`);
  nameBytes.copy(h, 0);
  h.write(octal(mode, 8), 100, 'latin1');
  h.write(octal(0, 8), 108, 'latin1');
  h.write(octal(0, 8), 116, 'latin1');
  h.write(octal(size, 12), 124, 'latin1');
  h.write(octal(mtime, 12), 136, 'latin1');
  h.write('        ', 148, 'latin1');
  h.write(type, 156, 'latin1');
  h.write('ustar\0', 257, 'latin1');
  h.write('00', 263, 'latin1');
  let sum = 0;
  for (const b of h) sum += b;
  h.write(`${sum.toString(8).padStart(6, '0')}\0 `, 148, 'latin1');
  return h;
}

const blocks = [];
function add(rel) {
  const full = path.join(parent, rel);
  const st = fs.lstatSync(full);
  const entry = rel.split(path.sep).join('/');
  if (st.isDirectory()) {
    blocks.push(header(`${entry}/`, 0o755, 0, '5'));
    for (const child of fs.readdirSync(full).sort()) add(path.join(rel, child));
  } else if (st.isFile()) {
    const data = fs.readFileSync(full);
    blocks.push(header(entry, st.mode & 0o100 ? 0o755 : 0o644, data.length, '0'), data);
    const pad = (BLOCK - (data.length % BLOCK)) % BLOCK;
    if (pad) blocks.push(Buffer.alloc(pad));
  } else {
    throw new Error(`${full}: only regular files and directories are packed`);
  }
}
add(name);
blocks.push(Buffer.alloc(2 * BLOCK));
const gz = gzipSync(Buffer.concat(blocks), { level: 9 });
gz[9] = 255; // OS: unknown
fs.writeFileSync(out, gz);
console.log(out);
