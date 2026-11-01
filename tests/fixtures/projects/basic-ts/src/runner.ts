import { greet as hello } from './greet';

export class Runner {
  run(): string {
    return hello('world');
  }
  static create(): Runner {
    return new Runner();
  }
}

export default function main(): void {
  Runner.create().run();
}
