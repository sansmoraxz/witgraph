/**
 * WAVE text as the value a graph run in the page gives, as jco lifts it: what an attached
 * session's outputs are read as, so a viewer is given one shape whoever runs the graph.
 *
 * - A number is a number (`nan`, `inf` and `-inf` too: a 64-bit integer loses nothing a
 *   preview shows); a bool, a string and a char are themselves.
 * - A list and a tuple are an array.
 * - A record is an object, its fields camelCased; flags are an object of the set ones, `true`.
 * - `some(x)` is `x`, and `none` is `undefined`.
 * - A case with a payload (`ok` and `err` too) is `{ tag, val }`; one without, an enum's say,
 *   is its name.
 *
 * Text that does not parse is `undefined`.
 */
export function fromWave(text: string): unknown {
  try {
    const parser = new Parser(text);
    const value = parser.value();
    return parser.atEnd() ? value : undefined;
  } catch {
    return undefined;
  }
}

/** `observed-at` as jco names a field: `observedAt`. */
function camel(name: string): string {
  return name.replace(/-([a-z0-9])/g, (_, c: string) => c.toUpperCase());
}

class Parser {
  #at = 0;
  readonly #text: string;

  constructor(text: string) {
    this.#text = text;
  }

  atEnd(): boolean {
    this.#space();
    return this.#at === this.#text.length;
  }

  #space(): void {
    while (this.#at < this.#text.length && /\s/.test(this.#text[this.#at])) this.#at++;
  }

  #peek(): string | undefined {
    this.#space();
    return this.#text[this.#at];
  }

  #expect(c: string): void {
    if (this.#peek() !== c) throw new SyntaxError(`expected \`${c}\` at ${this.#at}`);
    this.#at++;
  }

  value(): unknown {
    const c = this.#peek();
    if (c === undefined) throw new SyntaxError('expected a value');
    if (c === '"') return this.#string();
    if (c === "'") return this.#char();
    if (c === '[') return this.#sequence(']');
    if (c === '(') return this.#sequence(')');
    if (c === '{') return this.#braces();
    if (c === '-' || c === '+' || /[0-9]/.test(c)) return this.#number();
    const name = this.#label();
    if (this.#peek() === '(') {
      this.#at++;
      const val = this.value();
      this.#expect(')');
      return name === 'some' ? val : { tag: name, val };
    }
    switch (name) {
      case 'true':
        return true;
      case 'false':
        return false;
      case 'none':
        return undefined;
      case 'nan':
        return Number.NaN;
      case 'inf':
        return Number.POSITIVE_INFINITY;
      default:
        return name;
    }
  }

  #label(): string {
    this.#space();
    const found = /^%?[A-Za-z][A-Za-z0-9-]*/.exec(this.#text.slice(this.#at));
    if (!found) throw new SyntaxError(`expected a name at ${this.#at}`);
    this.#at += found[0].length;
    return found[0].replace(/^%/, '');
  }

  #number(): number {
    const found = /^[-+]?(inf|nan|\d+(\.\d+)?([eE][-+]?\d+)?)/.exec(this.#text.slice(this.#at));
    if (!found) throw new SyntaxError(`expected a number at ${this.#at}`);
    this.#at += found[0].length;
    const sign = found[0].startsWith('-') ? -1 : 1;
    if (found[1] === 'inf') return sign * Number.POSITIVE_INFINITY;
    if (found[1] === 'nan') return Number.NaN;
    return Number(found[0]);
  }

  /** One character of a string or char, an escape read. */
  #character(): string {
    const c = this.#text[this.#at++];
    if (c === undefined) throw new SyntaxError('unterminated text');
    if (c !== '\\') return c;
    const e = this.#text[this.#at++];
    switch (e) {
      case 'n':
        return '\n';
      case 'r':
        return '\r';
      case 't':
        return '\t';
      case 'u': {
        const found = /^\{([0-9a-fA-F]+)\}/.exec(this.#text.slice(this.#at));
        if (!found) throw new SyntaxError(`a bad escape at ${this.#at}`);
        this.#at += found[0].length;
        return String.fromCodePoint(Number.parseInt(found[1], 16));
      }
      case undefined:
        throw new SyntaxError('unterminated text');
      default:
        return e;
    }
  }

  #string(): string {
    this.#expect('"');
    let text = '';
    while (this.#text[this.#at] !== '"') text += this.#character();
    this.#at++;
    return text;
  }

  #char(): string {
    this.#expect("'");
    const c = this.#character();
    if (this.#text[this.#at++] !== "'") throw new SyntaxError('a char of more than one');
    return c;
  }

  #sequence(close: string): unknown[] {
    this.#at++;
    const items: unknown[] = [];
    while (this.#peek() !== close) {
      items.push(this.value());
      if (this.#peek() === ',') this.#at++;
      else break;
    }
    this.#expect(close);
    return items;
  }

  /** A record, or flags: `{a: 1}`, `{a, b}`, `{}`. */
  #braces(): Record<string, unknown> {
    this.#at++;
    const out: Record<string, unknown> = {};
    while (this.#peek() !== '}') {
      const name = camel(this.#label());
      if (this.#peek() === ':') {
        this.#at++;
        out[name] = this.value();
      } else {
        out[name] = true;
      }
      if (this.#peek() === ',') this.#at++;
      else break;
    }
    this.#expect('}');
    return out;
  }
}
