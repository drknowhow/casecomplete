import React from 'react';
import {AbsoluteFill, interpolate, interpolateColors, spring, useCurrentFrame, useVideoConfig} from 'remotion';
import {C, F, type Layout} from '../theme';
import {buildKeys, replay} from '../lib/typing';
import {SquiggleText} from '../components/text';
import {Stat, Win, pulseAt} from '../components/ui';
import {Kicker} from '../components/brand';

const clamp = {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'} as const;

export const HOOK_DUR = 195;
const TITLE_AT = 104;

// A fast typist fighting their own typos: type, notice, hold backspace, retype.
const KEYS = buildKeys(
  6,
  [
    {type: 'I hahve '},
    {pause: 7},
    {back: 6},
    {type: 'have been stuck on thsi '},
    {pause: 6},
    {back: 5},
    {type: 'this for a whiel becuase '},
    {pause: 5},
    {back: 14},
    {type: 'while becuase teh rulee '},
    {pause: 4},
    {back: 2},
    {type: ' is easty to usee'},
    {pause: 5},
    {back: 4},
    {type: 'use...'},
  ],
  'hook',
  1.7,
);

const TYPOS = new Set(['hahve', 'thsi', 'whiel', 'becuase', 'teh', 'rulee', 'easty', 'usee']);
const BACK_TIMES = KEYS.events.filter((e) => e.kind === 'back').map((e) => e.t);

const fmtClock = (frames: number) => {
  const s = Math.max(0, frames) / 30;
  const whole = Math.floor(s);
  return `0:${String(whole).padStart(2, '0')}.${Math.floor((s - whole) * 10)}`;
};

const ChatWindow: React.FC<{width: number; frame: number; fontSize: number}> = ({width, frame, fontSize}) => {
  const {text, lastT} = replay(KEYS.events, frame);
  return (
    <Win title="# parser-team" width={width}>
      <div style={{padding: '30px 40px 34px', display: 'flex', flexDirection: 'column', gap: 30}}>
        <div style={{display: 'flex', gap: 22, alignItems: 'flex-start'}}>
          <div
            style={{
              width: 60,
              height: 60,
              borderRadius: 30,
              background: C.orange,
              color: C.ink,
              fontFamily: F.display,
              fontWeight: 600,
              fontSize: 30,
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
              flexShrink: 0,
            }}
          >
            M
          </div>
          <div style={{fontFamily: F.body}}>
            <div style={{fontSize: 26, color: C.ink, fontWeight: 600}}>
              Maya <span style={{color: C.ghost, fontWeight: 400, fontSize: 22, marginLeft: 10}}>10:42</span>
            </div>
            <div style={{fontSize: 36, color: C.ink, marginTop: 4}}>Any luck with the lint rule?</div>
          </div>
        </div>
        <div
          style={{
            border: `2px solid ${C.paperHair}`,
            borderRadius: 18,
            background: '#FFFFFF',
            padding: '22px 28px',
            minHeight: fontSize * 1.42 * 3,
            fontFamily: F.body,
            fontSize,
            lineHeight: 1.42,
          }}
        >
          <SquiggleText text={text} typos={TYPOS} frame={frame} lastKey={lastT} ink={C.ink} caret={C.ink} />
        </div>
        <div style={{fontFamily: F.body, fontSize: 21, color: C.ghost, marginTop: -14}}>Enter to send</div>
      </div>
    </Win>
  );
};

export const Hook: React.FC<{layout: Layout}> = ({layout}) => {
  const frame = useCurrentFrame();
  const {fps} = useVideoConfig();
  const portrait = layout === 'portrait';
  const {backs} = replay(KEYS.events, frame);
  const backPulse = pulseAt(frame, BACK_TIMES, 6);
  const frustration = Math.min(1, backs / 30);
  const meterColor = interpolateColors(frustration, [0, 0.5, 1], [C.green, C.orange, C.red]);

  // Small shake while backspace is held.
  const shake = backPulse * Math.sin(frame * 2.7) * 4;

  const enter = spring({frame, fps, config: {damping: 200}, durationInFrames: 18});
  const titleIn = spring({frame: frame - TITLE_AT, fps, config: {damping: 16, mass: 0.7}});
  const title2In = spring({frame: frame - TITLE_AT - 14, fps, config: {damping: 16, mass: 0.7}});
  const dim = interpolate(frame, [TITLE_AT - 4, TITLE_AT + 10], [0, 1], clamp);

  const stats = [
    <Stat
      key="b"
      label="Backspaces"
      value={backs}
      sub="and counting"
      color={backs > 0 ? C.orange : C.paper}
      pulse={backPulse * 0.6}
      style={portrait ? {flex: 1} : undefined}
    />,
    <Stat
      key="t"
      label="Time on one line"
      value={fmtClock(frame - 6)}
      sub="still not sent"
      color={C.paper}
      style={portrait ? {flex: 1} : undefined}
      valueSize={portrait ? 64 : 76}
    />,
    <div
      key="m"
      style={{
        background: C.tile,
        border: `2px solid ${C.hair}`,
        borderRadius: 24,
        padding: '26px 32px',
        display: 'flex',
        flexDirection: 'column',
        gap: 18,
        ...(portrait ? {gridColumn: '1 / span 2'} : {}),
      }}
    >
      <div
        style={{
          fontFamily: F.body,
          fontWeight: 600,
          fontSize: 21,
          letterSpacing: '0.14em',
          textTransform: 'uppercase',
          color: C.ghost,
        }}
      >
        Frustration
      </div>
      <div style={{height: 22, borderRadius: 11, background: 'rgba(247,247,245,0.08)', overflow: 'hidden'}}>
        <div
          style={{
            width: `${6 + 94 * frustration}%`,
            height: '100%',
            borderRadius: 11,
            background: meterColor,
          }}
        />
      </div>
    </div>,
  ];

  return (
    <AbsoluteFill>
      <AbsoluteFill
        style={{
          padding: portrait ? '110px 60px' : '70px 120px',
          justifyContent: 'center',
          opacity: 1 - 0.84 * dim,
          filter: `blur(${dim * 10}px)`,
          transform: `scale(${1 - 0.03 * dim})`,
        }}
      >
        <Kicker text="Without" color={C.orange} />
        <div
          style={{
            display: 'flex',
            flexDirection: portrait ? 'column' : 'row',
            gap: portrait ? 36 : 56,
            marginTop: portrait ? 40 : 44,
            alignItems: 'flex-start',
            opacity: enter,
            transform: `translateY(${(1 - enter) * 40}px)`,
          }}
        >
          <div style={{transform: `translateX(${shake}px)`}}>
            <ChatWindow width={portrait ? 960 : 1180} frame={frame} fontSize={portrait ? 46 : 50} />
          </div>
          <div
            style={
              portrait
                ? {display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 24, width: 960}
                : {display: 'flex', flexDirection: 'column', gap: 24, width: 444}
            }
          >
            {stats}
          </div>
        </div>
      </AbsoluteFill>
      <AbsoluteFill style={{alignItems: 'center', justifyContent: 'center', textAlign: 'center'}}>
        <div style={{fontFamily: F.display, fontWeight: 600, fontSize: portrait ? 112 : 136, lineHeight: 1.08, letterSpacing: '-0.03em'}}>
          <div style={{color: C.paper, opacity: titleIn, transform: `translateY(${(1 - titleIn) * 50}px)`}}>
            Typing is fast.
          </div>
          <div style={{color: C.orange, opacity: title2In, transform: `translateY(${(1 - title2In) * 50}px)`}}>
            Fixing is slow.
          </div>
        </div>
      </AbsoluteFill>
    </AbsoluteFill>
  );
};
