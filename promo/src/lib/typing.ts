import {random} from 'remotion';

// Every typing effect is a pure function of the frame: a script is expanded once into
// timestamped key events, and the visible text is the replay of the events up to `frame`.

export type KeyEvent = {t: number; kind: 'char'; ch: string} | {t: number; kind: 'back'};
export type Step = {type: string; fpc?: number} | {back: number; fpb?: number} | {pause: number};

export const buildKeys = (
  start: number,
  steps: Step[],
  seed: string,
  fpc = 2,
): {events: KeyEvent[]; end: number} => {
  let t = start;
  let i = 0;
  const events: KeyEvent[] = [];
  for (const s of steps) {
    if ('pause' in s) {
      t += s.pause;
    } else if ('back' in s) {
      for (let k = 0; k < s.back; k++) {
        events.push({t: Math.round(t), kind: 'back'});
        t += s.fpb ?? 1.25;
      }
    } else {
      const step = s.fpc ?? fpc;
      for (const ch of s.type) {
        events.push({t: Math.round(t), kind: 'char', ch});
        t += step + (random(`${seed}-${i++}`) - 0.5) * step * 0.9;
      }
    }
  }
  return {events, end: Math.round(t)};
};

export const replay = (events: KeyEvent[], frame: number) => {
  let text = '';
  let backs = 0;
  let lastT = -1000;
  for (const e of events) {
    if (e.t > frame) break;
    if (e.kind === 'char') text += e.ch;
    else {
      text = text.slice(0, -1);
      backs++;
    }
    lastT = e.t;
  }
  return {text, backs, lastT};
};

// ---- Autocorrect documents: words that get fixed on space, then ghost completions ----

export type TypedWord = {typed: string; fixed?: string; charT: number[]; spaceT: number; fixT: number};
export type Ghost = {text: string; showT: number; acceptT: number};
export type Part = {kind: 'words'; words: TypedWord[]} | {kind: 'ghost'; ghost: Ghost};

// Fix latency: 118 ms at 30 fps is ~3.5 frames.
export const FIX_LATENCY = 4;

/** "I hahve>have been" -> words; "typo>fix" marks a word that gets autocorrected. */
export const typeWords = (
  start: number,
  spec: string,
  seed: string,
  fpc = 2,
): {part: Part; end: number} => {
  let t = start;
  let i = 0;
  const words: TypedWord[] = [];
  for (const token of spec.trim().split(/\s+/)) {
    const [typed, fixed] = token.split('>');
    const charT: number[] = [];
    for (let k = 0; k < typed.length; k++) {
      charT.push(Math.round(t));
      t += fpc + (random(`${seed}-${i++}`) - 0.5) * fpc * 0.9;
    }
    const spaceT = Math.round(t);
    t += fpc;
    words.push({typed, fixed, charT, spaceT, fixT: fixed ? spaceT + FIX_LATENCY : Infinity});
  }
  return {part: {kind: 'words', words}, end: words[words.length - 1].spaceT};
};

export type Seg =
  | {kind: 'plain'; text: string}
  | {kind: 'fixed'; text: string; at: number}
  | {kind: 'accepted'; text: string; at: number};

export type DocState = {
  segs: Seg[];
  ghost: {text: string; at: number} | null;
  lastKey: number;
  fixes: number;
  accepts: number;
};

export const docAt = (parts: Part[], f: number): DocState => {
  const segs: Seg[] = [];
  let lastKey = -1000;
  let fixes = 0;
  let accepts = 0;
  let ghost: DocState['ghost'] = null;
  outer: for (const p of parts) {
    if (p.kind === 'words') {
      for (const w of p.words) {
        const n = w.charT.filter((t) => t <= f).length;
        if (n === 0) break outer;
        lastKey = Math.max(lastKey, w.charT[n - 1]);
        if (n < w.typed.length || f < w.spaceT) {
          segs.push({kind: 'plain', text: w.typed.slice(0, n)});
          break outer;
        }
        lastKey = Math.max(lastKey, w.spaceT);
        if (w.fixed && f >= w.fixT) {
          segs.push({kind: 'fixed', text: w.fixed, at: w.fixT});
          fixes++;
        } else {
          segs.push({kind: 'plain', text: w.typed});
        }
        segs.push({kind: 'plain', text: ' '});
      }
    } else {
      const g = p.ghost;
      if (f < g.showT) break;
      if (f < g.acceptT) {
        ghost = {text: g.text, at: g.showT};
        break;
      }
      segs.push({kind: 'accepted', text: g.text, at: g.acceptT});
      accepts++;
      lastKey = Math.max(lastKey, g.acceptT);
    }
  }
  return {segs, ghost, lastKey, fixes, accepts};
};

/** All fix frames in a document, for stat pulses. */
export const fixTimes = (parts: Part[]) =>
  parts.flatMap((p) => (p.kind === 'words' ? p.words.filter((w) => w.fixed).map((w) => w.fixT) : []));
