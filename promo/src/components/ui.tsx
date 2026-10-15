import React from 'react';
import {interpolate, interpolateColors} from 'remotion';
import {C, F} from '../theme';
import {TabGlyph} from './brand';

const clamp = {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'} as const;

/** 0..1 key travel for a key pressed at each of `presses` (bottoms out on the press frame). */
export const pressDepth = (frame: number, presses: number[]) =>
  Math.max(0, ...presses.map((p) => interpolate(frame - p, [-3, 0, 3, 9], [0, 1, 1, 0], clamp)));

/** Pulse 0..1 that spikes on each event frame and decays. */
export const pulseAt = (frame: number, events: number[], len = 10) =>
  Math.max(0, ...events.map((e) => interpolate(frame - e, [0, 1, len], [0, 1, 0], clamp)));

export const TabKey: React.FC<{depth: number; scale?: number}> = ({depth, scale = 1}) => {
  const w = 210 * scale;
  const h = 118 * scale;
  const travel = 12 * scale;
  const face = interpolateColors(depth, [0, 1], ['#2B2F37', C.blue]);
  return (
    <div style={{position: 'relative', width: w, height: h + travel}}>
      <div
        style={{
          position: 'absolute',
          left: 0,
          top: travel,
          width: w,
          height: h,
          borderRadius: 22 * scale,
          background: '#08090B',
          boxShadow: `0 ${18 * (1 - depth) * scale}px ${40 * scale}px rgba(0,0,0,0.45)`,
        }}
      />
      <div
        style={{
          position: 'absolute',
          left: 0,
          top: travel * depth,
          width: w,
          height: h,
          boxSizing: 'border-box',
          borderRadius: 22 * scale,
          background: face,
          border: `${2 * scale}px solid rgba(255,255,255,${0.14 + 0.2 * depth})`,
          boxShadow: `0 0 ${60 * depth * scale}px rgba(58,123,213,${0.55 * depth})`,
          padding: `${16 * scale}px ${22 * scale}px`,
          display: 'flex',
          flexDirection: 'column',
          justifyContent: 'space-between',
        }}
      >
        <div style={{fontFamily: F.body, fontWeight: 600, fontSize: 30 * scale, color: C.paper, lineHeight: 1}}>Tab</div>
        <div style={{alignSelf: 'flex-end'}}>
          <TabGlyph size={64 * scale} color={C.paper} />
        </div>
      </div>
    </div>
  );
};

export const Stat: React.FC<{
  label: string;
  value: React.ReactNode;
  sub: React.ReactNode;
  color: string;
  pulse?: number;
  style?: React.CSSProperties;
  valueSize?: number;
}> = ({label, value, sub, color, pulse = 0, style, valueSize = 76}) => (
  <div
    style={{
      boxSizing: 'border-box',
      background: C.tile,
      border: `2px solid ${pulse > 0 ? `rgba(58,123,213,${0.2 + 0.6 * pulse})` : C.hair}`,
      borderRadius: 24,
      padding: '26px 32px',
      display: 'flex',
      flexDirection: 'column',
      gap: 8,
      ...style,
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
      {label}
    </div>
    <div
      style={{
        fontFamily: F.display,
        fontWeight: 600,
        fontSize: valueSize,
        lineHeight: 1.05,
        color,
        letterSpacing: '-0.02em',
        transform: `scale(${1 + 0.06 * pulse})`,
        transformOrigin: 'left center',
        fontVariantNumeric: 'tabular-nums',
        whiteSpace: 'nowrap',
      }}
    >
      {value}
    </div>
    <div style={{fontFamily: F.body, fontSize: 24, color: C.dim, whiteSpace: 'nowrap'}}>{sub}</div>
  </div>
);

const WinControls: React.FC<{color: string}> = ({color}) => (
  <svg width={110} height={20} viewBox="0 0 110 20" style={{display: 'block'}}>
    <path d="M4 10 H20" stroke={color} strokeWidth="2" strokeLinecap="round" />
    <rect x="47" y="3" width="14" height="14" rx="2" fill="none" stroke={color} strokeWidth="2" />
    <path d="M90 3 L104 17 M104 3 L90 17" stroke={color} strokeWidth="2" strokeLinecap="round" />
  </svg>
);

/** A Windows-style app window. */
export const Win: React.FC<{
  dark?: boolean;
  title: React.ReactNode;
  width: number;
  children: React.ReactNode;
  style?: React.CSSProperties;
  titleFont?: string;
}> = ({dark, title, width, children, style, titleFont = F.body}) => (
  <div
    style={{
      width,
      boxSizing: 'border-box',
      background: dark ? C.tile : C.paper,
      borderRadius: 22,
      border: `2px solid ${dark ? C.hairStrong : 'rgba(255,255,255,0.6)'}`,
      boxShadow: '0 40px 90px rgba(0,0,0,0.5), 0 8px 24px rgba(0,0,0,0.3)',
      overflow: 'hidden',
      ...style,
    }}
  >
    <div
      style={{
        height: 64,
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        padding: '0 28px',
        borderBottom: `2px solid ${dark ? C.hair : C.paperHair}`,
        background: dark ? '#16191D' : '#EEEEEB',
      }}
    >
      <div style={{fontFamily: titleFont, fontWeight: 600, fontSize: 24, color: dark ? C.dim : C.paperDim}}>{title}</div>
      <WinControls color={dark ? 'rgba(247,247,245,0.5)' : 'rgba(23,25,28,0.45)'} />
    </div>
    {children}
  </div>
);
