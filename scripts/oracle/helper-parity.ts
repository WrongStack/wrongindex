import * as fs from 'node:fs';
import * as path from 'node:path';
import * as os from 'node:os';
/**
 * Compare real native/producer databases with Python and Go present and absent.
 * Usage: bun scripts/oracle/helper-parity.ts <daemon> <clean-oracle> <fresh-output>
 * Only disposable system-temp fixtures are indexed; project code is never run.
 * Persistent helper failure and missing tree-sitter runtimes are separate cases.
 */
import {spawnSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {fileURLToPath} from 'node:url';
import {Database} from 'bun:sqlite';

if(process.argv[2]==='--probe'){
 const base=path.join(process.argv[3]!, 'packages/tools/src/codebase-index');
 const {resolvePythonBinary}=await import(path.join(base,'py-parser.ts'));
 const go=spawnSync('go',['version'],{encoding:'utf8',windowsHide:true,timeout:7000});
 console.log(JSON.stringify({python:await resolvePythonBinary(),goAvailable:go.status===0}));
 process.exit(0);
}
if(process.argv.length!==5)throw Error('Usage: helper-parity.ts <daemon> <clean-oracle> <fresh-output>');
const [daemon,engine,resultRoot]=process.argv.slice(2).map(p=>path.resolve(p));
const provenance=JSON.parse(fs.readFileSync(path.join(engine,'.wrongindex-oracle.json'),'utf8'));
if(provenance.schema!=='wrongindex.clean-oracle/1'||!/^[a-f0-9]{40}$/.test(provenance.commit)||!/^[a-f0-9]{64}$/.test(provenance.archiveSha256)||provenance.overlay||provenance.sourceState==='immutable-archive-with-recorded-overlay')throw Error('A recorded commit-only clean oracle is required');
if(fs.existsSync(resultRoot))throw Error('Preserve previous qualification');
fs.mkdirSync(resultRoot,{recursive:true});
const work=fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(),'.tmp-wi-degraded-')));
const emptyPath=path.join(work,'no-helpers');fs.mkdirSync(emptyPath);
const hash=(file:string)=>createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const baseEnv={...process.env,WRONGSTACK_HOME:path.join(work,'wrongstack'),WRONGINDEX_TOOLCHAIN_CACHE:''};
delete baseEnv.WRONGINDEX_NATIVE_STRICT;
const report:any={schema:'wrongindex.degraded-parity/1',status:'RUNNING',startedAt:new Date().toISOString(),work,producerCommit:provenance.commit,producerArchiveSha256:provenance.archiveSha256,daemonSha256:hash(daemon),scriptSha256:hash(import.meta.path),cases:[],boundary:'Python/Go availability only; no claim about persistent helper failures, missing JS runtimes or rendered GUI',normalization:'Existing logical dbdump: file timestamps, metadata time/random values, FTS physical tables only; key/table presence and FTS rowid retained'};
const save=()=>fs.writeFileSync(path.join(resultRoot,'result.json'),JSON.stringify(report,null,2)+'\n');
save();
function run(exe:string,args:string[],env:NodeJS.ProcessEnv,input:string|undefined,label:string){
 const r=spawnSync(exe,args,{env,cwd:emptyPath,input,encoding:'utf8',windowsHide:true,timeout:180000,maxBuffer:16*1024*1024});
 fs.writeFileSync(path.join(resultRoot,label+'.stdout'),r.stdout??'');fs.writeFileSync(path.join(resultRoot,label+'.stderr'),r.stderr??'');
 if(r.status!==0)throw Error(label+' process failed: '+(r.error?.message??r.status));
 return JSON.parse(r.stdout.trim());
}
function dump(file:string){
 const db=new Database(file,{readonly:true});db.exec('PRAGMA query_only=1');
 try{
  const tables=db.query("SELECT name,sql FROM sqlite_master WHERE type='table' ORDER BY name").all() as {name:string,sql:string}[];
  const out:Record<string,unknown>={};
  for(const {name,sql}of tables){
   if(['symbols_fts_config','symbols_fts_content','symbols_fts_data','symbols_fts_docsize','symbols_fts_idx'].includes(name)){out[name]=['<ignored>'];continue;}
   const rows=db.query(`SELECT ${sql?.startsWith('CREATE VIRTUAL TABLE')?'rowid AS rowid, ':''}* FROM "${name.replaceAll('"','""')}"`).all() as Record<string,unknown>[];
   for(const row of rows){
    if(name==='files')for(const key of ['mtime_ms','last_indexed'])if(Object.hasOwn(row,key))row[key]='<volatile>';
    if(name==='metadata'&&['graph_stamp','last_indexed','relation_epoch'].includes(row.key as string))row.value='<volatile>';
    for(const key of Object.keys(row))if(row[key] instanceof Uint8Array)row[key]={blob:Buffer.from(row[key] as Uint8Array).toString('hex')};
   }
   out[name]=rows.map(r=>Object.fromEntries(Object.entries(r).sort(([a],[b])=>a.localeCompare(b)))).sort((a,b)=>JSON.stringify(a).localeCompare(JSON.stringify(b)));
  }
  return out;
 }finally{db.close();}
}
try{
 for(const mode of ['available','unavailable']){
  const env=mode==='available'?baseEnv:{...baseEnv,PATH:emptyPath,Path:emptyPath};
  const probe=run(process.execPath,[import.meta.path,'--probe',engine],env,undefined,mode+'-probe');
  if(mode==='unavailable'&&(probe.python!==null||probe.goAvailable))throw Error('Missing-helper precondition failed: '+JSON.stringify(probe));
  if(mode==='available'&&(!probe.python||!probe.goAvailable))throw Error('Working-helper control unavailable');
  const project=path.join(work,mode,'project'),native=path.join(work,mode,'native'),oracle=path.join(work,mode,'oracle');fs.mkdirSync(project,{recursive:true});
  const write=(name:string,body:string)=>fs.writeFileSync(path.join(project,name),body);
  write('data.json','{"fixture":1}');
  write('a.py','import os\nclass User:\n    def get_name(self):\n        return "name"\ndef helper(value):\n    return value\n');
  write('bad.py','def broken(:\n pass\n');
  write('a.go','package demo\nimport "fmt"\ntype User struct { Name string }\nfunc Hello() { fmt.Println("hello") }\n');
  write('bad.go','package demo\nfunc Broken( {\n');
  const rounds=[{name:'fresh',files:null},{name:'targeted',files:['a.py','a.go']},{name:'reconcile',files:null},{name:'unchanged',files:null}];
  for(const round of rounds){
   if(round.name==='targeted'){
    write('a.py','import os\nclass User:\n    def get_name(self):\n        return "changed"\ndef renamed(value):\n    return value + 1\n');
    write('a.go','package demo\nimport "fmt"\ntype User struct { Name string; Age int }\nfunc Changed() { fmt.Println("changed") }\n');
   }
   const label=mode+'-'+round.name;
   const n=run(daemon,[round.files?'--native-update':'--native-rebuild',project,native],env,round.files?JSON.stringify(round.files):undefined,label+'-native');
   if(!n.ok)throw Error(label+' native failed: '+JSON.stringify(n));
   const o=run(process.execPath,[fileURLToPath(new URL('./index-fixture.ts',import.meta.url)),'--engine',engine],env,JSON.stringify({id:1,op:'index',args:{projectRoot:project,indexDir:oracle,...(round.files?{files:round.files}:{})}})+'\n',label+'-oracle');
   if(!o.ok)throw Error(label+' oracle failed: '+JSON.stringify(o));
   const a=dump(path.join(oracle,'index.db')),b=dump(path.join(native,'index.db'));
   fs.writeFileSync(path.join(resultRoot,label+'-oracle.json'),JSON.stringify(a,null,2));fs.writeFileSync(path.join(resultRoot,label+'-native.json'),JSON.stringify(b,null,2));
   const differentTables=[...new Set([...Object.keys(a),...Object.keys(b)])].sort().filter(k=>JSON.stringify(a[k])!==JSON.stringify(b[k]));
   report.cases.push({name:label,status:differentTables.length?'DIFFERENCE':'PASS',probe,native:n.result,reference:o.result,differentTables});save();
   console.log(label+': '+(differentTables.length?'DIFFERENCE '+differentTables.join(', '):'PASS'));
  }
 }
 report.status=report.cases.some((c:any)=>c.status!=='PASS')?'DIFFERENCE':'PASS';
 if(report.status!=='PASS')process.exitCode=1;
}catch(e){report.status='FAIL';report.error=(e as Error).message;process.exitCode=1;}
finally{report.finishedAt=new Date().toISOString();save();}
