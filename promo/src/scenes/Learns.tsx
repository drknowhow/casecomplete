import React from 'react';
import {AbsoluteFill, Easing, interpolate, spring, useCurrentFrame, useVideoConfig} from 'remotion';
import {C, F, type Layout} from '../theme';
import {Arrow, Kicker} from '../components/brand';

const clamp = {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'} as const;

export const LEARNS_DUR = 192;

const Chip: React.FC<{
  frame: number;
  at: number;
  tag: string;
  tagColor: string;
  tagBg: string;
  sub: string;
  width: number;
  children: React.ReactNode;
}> = ({frame, at, tag, tagColor, tagBg, sub, width, children}) => {
  const {fps} = useVideoConfig();
  const s = spring({frame: frame - at, fps, config: {damping: 15, mass: 0.6}});
  return (
    <div
      style={{
        width,
        boxSizing: 'border-box',
        background: C.tile,
        border: `2px solid ${C.hair}`,
        borderLeft: `6px solid ${tagColor}`,
        borderRadius: 22,
        padding: '24px 32px',
        opacity: s,
        transform: `translateX(${(1 - s) * -60}px)`,
      }}
    >
      <div style={{display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 24}}>
        <div style={{fontFamily: F.body, fontSize: 40, color: C.paper, display: 'flex', alignItems: 'center', gap: 16}}>
          {children}
        </div>
        <div
          style={{
            fontFamily: F.body,
            fontWeight: 600,
            fontSize: 21,
            letterSpacing: '0.14em',
            textTransform: 'uppercase',
            color: tagColor,
            background: tagBg,
            borderRadius: 999,
            padding: '8px 18px',
            whiteSpace: 'nowrap',
          }}
        >
          {tag}
        </div>
      </div>
      <div style={{fontFamily: F.body, fontSize: 24, color: C.dim, marginTop: 8}}>{sub}</div>
    </div>
  );
};

const DATA = [52, 57, 63, 68, 74, 79, 84, 88];

const Chart: React.FC<{frame: number; width: number; height: number}> = ({frame, width, height}) => {
  const p = interpolate(frame, [34, 150], [0, 1], {...clamp, easing: Easing.bezier(0.33, 0, 0.25, 1)});
  const pad = {l: 70, r: 30, t: 20, b: 50};
  const w = width - pad.l - pad.r;
  const h = height - pad.t - pad.b;
  const y = (v: number) => pad.t + h - ((v - 40) / 60) * h;
  const x = (i: number) => pad.l + (i / (DATA.length - 1)) * w;
  const line = DATA.map((v, i) => `${i ? 'L' : 'M'}${x(i)} ${y(v)}`).join(' ');
  const area = `${line} L${x(DATA.length - 1)} ${pad.t + h} L${x(0)} ${pad.t + h} Z`;
  const fi = p * (DATA.length - 1);
  const i0 = Math.min(DATA.length - 2, Math.floor(fi));
  const v = DATA[i0] + (DATA[i0 + 1] - DATA[i0]) * (fi - i0);
  const cx = pad.l + p * w;
  return (
    <div style={{display: 'flex', flexDirection: 'column', gap: 6}}>
      <div style={{display: 'flex', alignItems: 'baseline', justifyContent: 'space-between'}}>
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
          Suggestions you accept
        </div>
        <div style={{fontFamily: F.display, fontWeight: 600, fontSize: 76, color: C.paper, fontVariantNumeric: 'tabular-nums'}}>
          {Math.round(v)}%
        </div>
      </div>
      <svg width={width} height={height} style={{display: 'block', overflow: 'visible'}}>
        <defs>
          <linearGradient id="fillg" x1="0" x2="0" y1="0" y2="1">
            <stop offset="0" stopColor={C.blue} stopOpacity="0.38" />
            <stop offset="1" stopColor={C.blue} stopOpacity="0" />
          </linearGradient>
          <clipPath id="reveal">
            <rect x={0} y={0} width={cx} height={height} />
          </clipPath>
        </defs>
        {[50, 75, 100].map((g) => (
          <g key={g}>
            <line x1={pad.l} x2={pad.l + w} y1={y(g)} y2={y(g)} stroke={C.hair} strokeWidth={2} />
            <text x={pad.l - 14} y={y(g) + 8} textAnchor="end" fontFamily={F.body} fontSize={22} fill={C.ghost}>
              {g}%
            </text>
          </g>
        ))}
        <g clipPath="url(#reveal)">
          <path d={area} fill="url(#fillg)" />
          <path d={line} fill="none" stroke={C.blue} strokeWidth={6} strokeLinejoin="round" strokeLinecap="round" />
        </g>
        <circle cx={cx} cy={y(v)} r={11} fill={C.paper} stroke={C.blue} strokeWidth={5} opacity={p > 0 ? 1 : 0} />
        <text x={pad.l} y={height - 8} fontFamily={F.body} fontSize={22} fill={C.ghost}>
          Week 1
        </text>
        <text x={pad.l + w} y={height - 8} textAnchor="end" fontFamily={F.body} fontSize={22} fill={C.ghost}>
          Week 8
        </text>
      </svg>
    </div>
  );
};

export const Learns: React.FC<{layout: Layout}> = ({layout}) => {
  const frame = useCurrentFrame();
  const {fps} = useVideoConfig();
  const portrait = layout === 'portrait';
  const head = spring({frame, fps, config: {damping: 200}, durationInFrames: 18});
  const chartIn = spring({frame: frame - 24, fps, config: {damping: 200}, durationInFrames: 20});
  const chipW = portrait ? 960 : 860;
  const mono: React.CSSProperties = {fontFamily: F.mono, fontSize: 38};

  return (
    <AbsoluteFill style={{padding: portrait ? '110px 60px' : '70px 120px', justifyContent: 'center'}}>
      <Kicker text="Learns" color={C.greenText} />
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
        It learns how you type.
      </div>
      <div
        style={{
          display: 'flex',
          flexDirection: portrait ? 'column' : 'row',
          gap: portrait ? 56 : 60,
          marginTop: portrait ? 56 : 60,
          alignItems: 'flex-start',
        }}
      >
        <div style={{display: 'flex', flexDirection: 'column', gap: 24}}>
          <Chip
            frame={frame}
            at={14}
            tag="learned"
            tagColor={C.greenText}
            tagBg="rgba(46,139,87,0.2)"
            sub="You retyped it, so now it fixes itself."
            width={chipW}
          >
            <span style={{...mono, color: C.ghost}}>dimitr</span>
            <Arrow size={44} color={C.greenText} />
            <span style={{...mono, color: C.paper, fontWeight: 600}}>Dimitri</span>
          </Chip>
          <Chip
            frame={frame}
            at={36}
            tag="added"
            tagColor="#6FA2EA"
            tagBg="rgba(58,123,213,0.2)"
            sub="You undid the fix, so it is a word now."
            width={chipW}
          >
            <span>
              <span style={{...mono, fontWeight: 600}}>&lsquo;yepgent&rsquo;</span> added to your words
            </span>
          </Chip>
          <Chip
            frame={frame}
            at={58}
            tag="undone"
            tagColor={C.orange}
            tagBg="rgba(240,163,94,0.18)"
            sub="A fix you keep undoing backs off."
            width={chipW}
          >
            <span>Rule demoted after 6 undos</span>
          </Chip>
        </div>
        <div
          style={{
            boxSizing: 'border-box',
            width: portrait ? 960 : 760,
            background: C.tile,
            border: `2px solid ${C.hair}`,
            borderRadius: 24,
            padding: '26px 34px 30px',
            opacity: chartIn,
            transform: `translateY(${(1 - chartIn) * 30}px)`,
          }}
        >
          <Chart frame={frame} width={portrait ? 888 : 688} height={portrait ? 400 : 330} />
        </div>
      </div>
    </AbsoluteFill>
  );
};
