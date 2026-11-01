import { Runner } from './runner';

export function View(props: { label: string }) {
  const r = new Runner();
  return <div title={props.label}>{r.run()}</div>;
}
