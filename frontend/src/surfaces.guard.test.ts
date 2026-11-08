/// <reference types="vite/client" />
// Guards against menus, sidebars and popups rendering see-through and bleeding into
// whatever sits underneath them. Two failure modes caused that historically:
//   1. A color utility referencing a token missing from `@theme` (e.g. `bg-sidebar`)
//      silently compiles to nothing, so the element has no background at all.
//   2. A layered element (positioned + z-index) with no opaque background.
import { describe, expect, it } from 'vitest';
import css from './globals.css?raw';

const sources = import.meta.glob(['./**/*.{ts,tsx}', '!./**/*.test.ts'], {
  query: '?raw', import: 'default', eager: true,
}) as Record<string, string>;

const themeTokens = new Set([...css.matchAll(/--color-([a-z0-9-]+)\s*:/g)].map(m => m[1]));

const PALETTE = /^(slate|gray|zinc|neutral|stone|red|orange|amber|yellow|lime|green|emerald|teal|cyan|sky|blue|indigo|violet|purple|fuchsia|pink|rose)-\d{2,3}$/;
const KEYWORDS = new Set(['transparent', 'current', 'inherit', 'black', 'white']);
// Non-color suffixes per prefix (sizes, sides, styles, alignment, ...).
const NON_COLOR: Record<string, RegExp> = {
  bg: /^(none|fixed|local|scroll|repeat.*|no-repeat|cover|contain|auto|center|top|bottom|left|right|clip-.+|origin-.+|blend-.+|linear-.+|radial.*|conic.*|gradient-.+|\[.+\])$/,
  text: /^(xs|sm|base|lg|[2-9]?xl|left|center|right|start|end|justify|wrap|nowrap|balance|pretty|ellipsis|clip|\[.+\])$/,
  border: /^([0-9]+|[tblrxyse]|[tblrxyse]-[0-9]+|solid|dashed|dotted|double|hidden|none|collapse|separate|spacing.*|box|\[.+\])$/,
  divide: /^([xy]|[xy]-[0-9]+|[xy]-reverse|solid|dashed|dotted|double|none)$/,
  outline: /^([0-9]+|none|hidden|solid|dashed|dotted|double|offset-.+)$/,
  ring: /^([0-9]+|inset|offset-.+)$/,
  fill: /^none$/, stroke: /^([0-9]+|none)$/,
};
const COLOR_PREFIX = /^(bg|text|border(?:-[tblrxyse])?|divide|outline|ring|fill|stroke|placeholder|caret|accent|from|via|to|decoration|shadow)-(.+)$/;

function classTokens(src: string) {
  return src.split('\n').flatMap((line, i) =>
    line.split(/[\s"'`{}(),;<>=]+/).map(raw => ({ token: raw.split(':').pop()!.replace(/^[!-]/, ''), line: i + 1 })));
}

describe('surface color tokens', () => {
  it('every custom color utility resolves to a token registered in @theme', () => {
    const unknown: string[] = [];
    for (const [file, src] of Object.entries(sources)) {
      for (const { token, line } of classTokens(src)) {
        const m = COLOR_PREFIX.exec(token);
        if (!m) continue;
        const prefix = m[1].replace(/^border-.*/, 'border');
        const value = m[2].replace(/\/[0-9]+$/, '');
        if (NON_COLOR[prefix]?.test(m[2]) || KEYWORDS.has(value) || PALETTE.test(value)) continue;
        if (['from', 'via', 'to', 'decoration', 'shadow', 'accent', 'caret', 'placeholder'].includes(prefix) && !themeTokens.has(value)) {
          // These prefixes double as non-color utilities (shadow-md, accent-auto, ...): only flag token-shaped names.
          if (!/^(background|foreground|card|popover|primary|secondary|muted|accent|destructive|success|border|input|ring|sidebar)/.test(value)) continue;
        }
        if (!themeTokens.has(value)) unknown.push(`${file}:${line} ${token}`);
      }
    }
    expect(unknown, 'color utilities referencing tokens missing from @theme in globals.css render with NO color').toEqual([]);
  });

  it('surface tokens are fully opaque in light and dark themes', () => {
    const blocks = [...css.matchAll(/(:root|\.dark)\s*\{([^}]*)\}/g)];
    expect(blocks.length).toBe(2);
    const translucent: string[] = [];
    for (const [, scope, body] of blocks) {
      for (const name of ['background', 'card', 'popover', 'sidebar', 'muted', 'secondary', 'accent']) {
        const value = new RegExp(`--${name}:\\s*([^;]+);`).exec(body)?.[1].trim();
        if (!value) { translucent.push(`${scope} --${name} missing`); continue; }
        if (/transparent|rgba|hsla|\/|^#[0-9a-f]{8}$|^#[0-9a-f]{4}$/i.test(value)) translucent.push(`${scope} --${name}: ${value}`);
      }
    }
    expect(translucent).toEqual([]);
  });
});

describe('layered surfaces', () => {
  it('every positioned element with a z-index paints an opaque background (scrims excepted)', () => {
    const offenders: string[] = [];
    for (const [file, src] of Object.entries(sources)) {
      for (const [literal] of src.matchAll(/(["'`])(?:(?!\1).)*\bz-(?:\d+|\[\d+\])(?:(?!\1).)*\1/g)) {
        const classes = literal.slice(1, -1).split(/\s+/);
        if (!classes.some(c => /^(fixed|absolute|sticky|relative)$/.test(c))) continue;
        const isScrim = classes.includes('inset-0') && classes.some(c => /^bg-black\/\d+$/.test(c));
        if (isScrim) continue;
        const opaque = classes.some(c => /^bg-[a-z-]+$/.test(c) && c !== 'bg-transparent' && (themeTokens.has(c.slice(3)) || c === 'bg-black' || c === 'bg-white'));
        if (!opaque) offenders.push(`${file}: ${literal}`);
      }
    }
    expect(offenders, 'layered elements without an opaque token background let content show through').toEqual([]);
  });

  it('floating Radix primitives are only used through the vetted wrappers in components/ui', () => {
    const floating = /@radix-ui\/react-(popover|dropdown-menu|select|tooltip|hover-card|context-menu|menubar|navigation-menu|dialog|alert-dialog)/;
    const leaks = Object.entries(sources)
      .filter(([file, src]) => floating.test(src) && !file.startsWith('./components/ui/'))
      .map(([file]) => file);
    expect(leaks, 'import the wrapper from components/ui so content gets bg-popover/bg-card and a z-index').toEqual([]);
  });
});
