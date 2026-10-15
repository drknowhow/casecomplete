import React from 'react';
import {AbsoluteFill, interpolate, spring, useCurrentFrame, useVideoConfig} from 'remotion';
import {C, F, type Layout} from '../theme';
import {buildKeys, replay} from '../lib/typing';
import {Caret} from '../components/text';
import {Kicker, Shield} from '../components/brand';
import {pulseAt} from '../components/ui';

const clamp = {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'} as const;

export const PRIVATE_DUR = 192;

const CARD = buildKeys(24, [{type: '4111 1111 1111 1111'}], 'card', 2.4);
// The field is recognised as a card number on the first digit: the shield is up before anything could leave.
const DETECT = CARD.events[0].t;
const KEY_T = CARD.events.map((e) => e.t);
const DOT_FRAMES = 11;

const Chip: React.FC<{text: React.ReactNode; o: number}> = ({text, o}) => (
  <div
    style={{
      display: 'flex',
      alignItems: 'center',
      gap: 14,
      background: C.tile,
      border: `2px solid ${C.hair}`,
      borderRadius: 999,
      padding: '14px 26px',
      fontFamily: F.body,
      fontSize: 28,
      color: C.paper,
      opacity: o,
      transform: `translateY(${(1 - o) * 16}px)`,
      whiteSpace: 'nowrap',
    }}
  >
    <Shield size={30} color={C.green} />
    {text}
  </div>
);

const GpuChip: React.FC<{size: number}> = ({size}) => (
  <svg width={size} height={size} viewBox="0 0 64 64" style={{display: 'block'}}>
    {[16, 26, 38, 48].map((p) => (
      <g key={p} stroke={C.ghost} strokeWidth="3" strokeLinecap="round">
        <path d={`M${p} 4 V12 M${p} 52 V60 M4 ${p} H12 M52 ${p} H60`} />
      </g>
    ))}
    <rect x="12" y="12" width="40" height="40" rx="8" fill="#23272E" stroke={C.ghost} strokeWidth="3" />
    <rect x="22" y="22" width="20" height="20" rx="4" fill={C.blue} />
  </svg>
);

export const Private: React.FC<{layout: Layout}> = ({layout}) => {
  const frame = useCurrentFrame();
  const {fps} = useVideoConfig();
  const portrait = layout === 'portrait';
  const {text, lastT} = replay(CARD.events, frame);

  const head = spring({frame, fps, config: {damping: 200}, durationInFrames: 18});
  const demoIn = spring({frame: frame - 4, fps, config: {damping: 200}, durationInFrames: 18});
  const shieldIn = spring({frame: frame - DETECT, fps, config: {damping: 11, mass: 0.6}});
  const cut = interpolate(frame, [DETECT, DETECT + 10], [0, 1], clamp);
  const chipA = spring({frame: frame - 92, fps, config: {damping: 200}, durationInFrames: 16});
  const chipB = spring({frame: frame - 104, fps, config: {damping: 200}, durationInFrames: 16});
  const tail = spring({frame: frame - 118, fps, config: {damping: 200}, durationInFrames: 18});

  // Each keystroke sends a dot toward the model; the shield absorbs every one.
  const dots = KEY_T.map((t, i) => ({t, key: i}))
    .filter(({t}) => frame >= t && frame < t + DOT_FRAMES)
    .map(({t, key}) => {
      const p = (frame - t) / DOT_FRAMES;
      return {p, o: interpolate(p, [0, 0.65, 1], [1, 1, 0], clamp), key};
    });
  const absorb = pulseAt(frame, KEY_T.map((t) => t + DOT_FRAMES - 2), 8);

  const field = (
    <div
      style={{
        boxSizing: 'border-box',
        width: portrait ? 960 : 700,
        background: C.paper,
        borderRadius: 22,
        padding: '26px 32px',
        boxShadow: '0 30px 70px rgba(0,0,0,0.45)',
      }}
    >
      <div style={{fontFamily: F.body, fontSize: 24, fontWeight: 600, color: C.paperDim}}>Card number</div>
      <div
        style={{
          marginTop: 12,
          border: `2px solid ${frame >= DETECT ? 'rgba(46,139,87,0.8)' : C.paperHair}`,
          borderRadius: 14,
          background: '#fff',
          padding: '14px 22px',
          fontFamily: F.mono,
          fontSize: portrait ? 50 : 46,
          color: C.ink,
          minHeight: (portrait ? 50 : 46) * 1.35,
          lineHeight: 1.35,
          whiteSpace: 'pre',
        }}
      >
        {text}
        <Caret frame={frame} lastKey={lastT} color={C.ink} />
      </div>
    </div>
  );

  const model = (
    <div
      style={{
        boxSizing: 'border-box',
        width: portrait ? 960 : 520,
        background: C.tile,
        border: `2px solid ${C.hairStrong}`,
        borderRadius: 22,
        padding: '28px 32px',
        display: 'flex',
        alignItems: 'center',
        gap: 26,
      }}
    >
      <GpuChip size={84} />
      <div>
        <div style={{fontFamily: F.display, fontWeight: 600, fontSize: 44, color: C.paper, lineHeight: 1.1}}>
          Gemma 3 1B
        </div>
        <div style={{fontFamily: F.body, fontSize: 26, color: C.dim, marginTop: 6}}>The model, on your GPU</div>
      </div>
    </div>
  );

  const len = portrait ? 230 : 380;
  const connector = (
    <div
      style={{
        position: 'relative',
        width: portrait ? 960 : len,
        height: portrait ? len : 220,
        flexShrink: 0,
      }}
    >
      <svg
        width={portrait ? 960 : len}
        height={portrait ? len : 220}
        style={{position: 'absolute', left: 0, top: 0, overflow: 'visible'}}
      >
        {portrait ? (
          <>
            <line x1={480} y1={0} x2={480} y2={len / 2} stroke={C.hairStrong} strokeWidth={4} />
            <line
              x1={480}
              y1={len / 2}
              x2={480}
              y2={len}
              stroke={frame >= DETECT ? 'rgba(247,247,245,0.14)' : C.hairStrong}
              strokeWidth={4}
              strokeDasharray={frame >= DETECT ? '10 12' : undefined}
            />
            {dots.map((d) => (
              <circle key={d.key} cx={480} cy={d.p * (len / 2 - 50)} r={9} fill={C.blue} opacity={d.o} />
            ))}
          </>
        ) : (
          <>
            <line x1={0} y1={110} x2={len / 2} y2={110} stroke={C.hairStrong} strokeWidth={4} />
            <line
              x1={len / 2}
              y1={110}
              x2={len}
              y2={110}
              stroke={frame >= DETECT ? 'rgba(247,247,245,0.14)' : C.hairStrong}
              strokeWidth={4}
              strokeDasharray={frame >= DETECT ? '10 12' : undefined}
            />
            {dots.map((d) => (
              <circle key={d.key} cx={d.p * (len / 2 - 50)} cy={110} r={9} fill={C.blue} opacity={d.o} />
            ))}
          </>
        )}
      </svg>
      <div
        style={{
          position: 'absolute',
          left: '50%',
          top: '50%',
          transform: `translate(-50%, -50%) scale(${shieldIn})`,
          display: 'flex',
          flexDirection: portrait ? 'row' : 'column',
          alignItems: 'center',
          gap: portrait ? 22 : 10,
        }}
      >
        {portrait ? <div style={{width: 210}} /> : null}
        <div
          style={{
            width: 96,
            height: 96,
            borderRadius: 48,
            background: C.night,
            border: `3px solid ${C.green}`,
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            boxShadow: `0 0 ${40 * cut + 30 * absorb}px rgba(46,139,87,${0.5 + 0.3 * absorb})`,
          }}
        >
          <Shield size={58} color={C.green} />
        </div>
        <div
          style={{
            fontFamily: F.body,
            fontWeight: 600,
            fontSize: 30,
            color: C.greenText,
            letterSpacing: '0.04em',
            whiteSpace: 'nowrap',
            width: portrait ? 210 : undefined,
            opacity: cut,
          }}
        >
          never sent
        </div>
      </div>
    </div>
  );

  return (
    <AbsoluteFill style={{padding: portrait ? '110px 60px' : '70px 120px', justifyContent: 'center'}}>
      <Kicker text="Private" color={C.greenText} />
      <div
        style={{
          fontFamily: F.display,
          fontWeight: 600,
          fontSize: portrait ? 92 : 84,
          lineHeight: 1.08,
          letterSpacing: '-0.03em',
          color: C.paper,
          marginTop: 22,
          opacity: head,
          transform: `translateY(${(1 - head) * 30}px)`,
        }}
      >
        Everything stays on your computer.
      </div>
      <div
        style={{
          display: 'flex',
          flexDirection: portrait ? 'column' : 'row',
          alignItems: 'center',
          marginTop: portrait ? 56 : 64,
          opacity: demoIn,
          transform: `translateY(${(1 - demoIn) * 30}px)`,
        }}
      >
        {field}
        {connector}
        {model}
      </div>
      <div
        style={{
          display: 'flex',
          flexDirection: portrait ? 'column' : 'row',
          gap: 20,
          marginTop: portrait ? 56 : 64,
          alignItems: 'flex-start',
        }}
      >
        <Chip o={chipA} text="Card numbers, SSNs, passwords: never sent to the model" />
        <Chip o={chipB} text="Banking and health sites: off by default" />
      </div>
      <div
        style={{
          marginTop: portrait ? 56 : 52,
          fontFamily: F.display,
          fontWeight: 500,
          fontSize: portrait ? 50 : 46,
          color: C.paper,
          opacity: tail,
          transform: `translateY(${(1 - tail) * 20}px)`,
        }}
      >
        Gemma 3 1B, local. <span style={{color: C.ghost}}>No cloud.</span>
      </div>
    </AbsoluteFill>
  );
};
