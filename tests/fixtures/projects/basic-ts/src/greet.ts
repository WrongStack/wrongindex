/** Greets a person by name. */
export function greet(name: string): string {
  return helper(name);
}

function helper(n: string): string {
  return `hi ${n}`;
}

export const çağrı = (x: number) => x * 2; // non-ASCII identifier
