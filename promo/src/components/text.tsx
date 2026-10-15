import React from 'react';
import {interpolate, interpolateColors} from 'remotion';
import {C} from '../theme';
import type {DocState} from '../lib/typing';

const clamp = {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'} as const;

/** Zero-width caret, so moving it never reflows the text around it. Blinks once idle. */
export const Caret: React.FC<{frame: number; lastKey: number; color: string; hidden?: boolean}> = ({
  frame,
  lastKey,
  color,
  hidden,
}) => {
  const idle = frame - lastKey;
  const on = idle < 12 ? 1 : Math.floor((idle - 12) / 15) % 2 === 0 ? 0 : 1;
  return (
    <span style={{display: 'inline-block', width: 0, height: '1em', position: 'relative'}}>
      <span
        style={{
          position: 'absolute',
          left: -1,
          top: '-0.04em',
          width: 3,
          height: '1.24em',
          borderRadius: 2,
          background: color,
          opacity: hidden ? 0 : on,
        }}
      />
    </span>
  );
};

/** A word autocorrected on space: slides in with a brief signal-blue underline and wash. */
const FixedWord: React.FC<{text: string; at: number; frame: number}> = ({text, at, frame}) => {
  const d = frame - at;
  const p = interpolate(d, [0, 5], [0, 1], clamp);
  const ul = interpolate(d, [0, 2, 22, 36], [0, 1, 1, 0], clamp);
  return (
    <span
      style={{
        display: 'inline-block',
        transform: `translateY(${(1 - p) * 0.32}em)`,
        opacity: 0.25 + 0.75 * p,
        textDecorationLine: 'underline',
        textDecorationColor: `rgba(58,123,213,${ul})`,
        textDecorationThickness: '0.1em',
        textUnderlineOffset: '0.2em',
        borderRadius: '0.12em',
        boxShadow: `0 0 0 0.08em rgba(58,123,213,${0.18 * ul})`,
        backgroundColor: `rgba(58,123,213,${0.18 * ul})`,
      }}
    >
      {text}
    </span>
  );
};

export const DocText: React.FC<{
  state: DocState;
  frame: number;
  ink: string;
  ghost: string;
  caret?: string;
  hideCaret?: boolean;
}> = ({state, frame, ink, ghost, caret = C.blue, hideCaret}) => {
  const g = state.ghost;
  let ghostText = '';
  let ghostOpacity = 0;
  if (g) {
    const reveal = interpolate(frame - g.at, [0, 4], [0.15, 1], clamp);
    ghostText = g.text.slice(0, Math.ceil(g.text.length * reveal));
    ghostOpacity = interpolate(frame - g.at, [0, 3], [0.3, 1], clamp);
  }
  return (
    <span style={{whiteSpace: 'pre-wrap', color: ink}}>
      {state.segs.map((s, i) => {
        if (s.kind === 'fixed') return <FixedWord key={i} text={s.text} at={s.at} frame={frame} />;
        if (s.kind === 'accepted') {
          const color = interpolateColors(frame - s.at, [0, 6], [ghost, ink]);
          return (
            <span key={i} style={{color}}>
              {s.text}
            </span>
          );
        }
        return <span key={i}>{s.text}</span>;
      })}
      <Caret frame={frame} lastKey={state.lastKey} color={caret} hidden={hideCaret} />
      {g ? <span style={{color: ghost, opacity: ghostOpacity}}>{ghostText}</span> : null}
    </span>
  );
};

/** Raw typed text with red spellcheck squiggles under finished misspelled words. */
export const SquiggleText: React.FC<{
  text: string;
  typos: Set<string>;
  frame: number;
  lastKey: number;
  ink: string;
  caret: string;
}> = ({text, typos, frame, lastKey, ink, caret}) => {
  const tokens = text.split(/( )/);
  return (
    <span style={{whiteSpace: 'pre-wrap', color: ink}}>
      {tokens.map((tok, i) => {
        const finished = i < tokens.length - 1;
        const bad = finished && typos.has(tok.replace(/[.,]+$/, ''));
        return (
          <span
            key={i}
            style={
              bad
                ? {
                    textDecorationLine: 'underline',
                    textDecorationStyle: 'wavy',
                    textDecorationColor: C.red,
                    textDecorationThickness: '0.06em',
                    textUnderlineOffset: '0.22em',
                  }
                : undefined
            }
          >
            {tok}
          </span>
        );
      })}
      <Caret frame={frame} lastKey={lastKey} color={caret} />
    </span>
  );
};
