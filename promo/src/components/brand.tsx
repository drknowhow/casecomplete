import React from 'react';
import {AbsoluteFill, interpolate, useCurrentFrame} from 'remotion';
import {C, F} from '../theme';

/** The Tab-key mark, from the 120-unit reference SVG. */
export const Mark: React.FC<{size: number; style?: React.CSSProperties}> = ({size, style}) => (
  <svg width={size} height={size} viewBox="0 0 120 120" style={{display: 'block', overflow: 'visible', ...style}}>
    <rect x="6" y="6" width="108" height="100" rx="22" fill={C.blue} />
    <path d="M30 56 H78" stroke="#fff" strokeWidth="11" strokeLinecap="round" />
    <path
      d="M64 38 L84 56 L64 74"
      fill="none"
      stroke="#fff"
      strokeWidth="11"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
    <path d="M92 36 V76" stroke="#fff" strokeWidth="11" strokeLinecap="round" />
  </svg>
);

/** The tab-arrow glyph alone, for keycaps. */
export const TabGlyph: React.FC<{size: number; color: string}> = ({size, color}) => (
  <svg width={size} height={(size * 52) / 76} viewBox="24 30 76 52" style={{display: 'block'}}>
    <path d="M30 56 H78" stroke={color} strokeWidth="10" strokeLinecap="round" />
    <path d="M64 38 L84 56 L64 74" fill="none" stroke={color} strokeWidth="10" strokeLinecap="round" strokeLinejoin="round" />
    <path d="M92 36 V76" stroke={color} strokeWidth="10" strokeLinecap="round" />
  </svg>
);

export const Wordmark: React.FC<{size: number; color?: string}> = ({size, color = C.paper}) => (
  <div style={{fontFamily: F.display, fontWeight: 600, fontSize: size, color, letterSpacing: '-0.02em', lineHeight: 1}}>
    CaseComplete
  </div>
);

export const Arrow: React.FC<{size: number; color: string}> = ({size, color}) => (
  <svg width={size} height={size * 0.6} viewBox="0 0 40 24" style={{display: 'inline-block', verticalAlign: 'middle'}}>
    <path d="M3 12 H34" stroke={color} strokeWidth="3.2" strokeLinecap="round" />
    <path d="M25 4 L35 12 L25 20" fill="none" stroke={color} strokeWidth="3.2" strokeLinecap="round" strokeLinejoin="round" />
  </svg>
);

export const Shield: React.FC<{size: number; color: string; check?: string}> = ({size, color, check = '#fff'}) => (
  <svg width={size} height={size} viewBox="0 0 64 64" style={{display: 'block'}}>
    <path d="M32 5 L54 13 V30 C54 44 45 54 32 59 C19 54 10 44 10 30 V13 Z" fill={color} />
    <path d="M22 32 L29 39 L43 24" fill="none" stroke={check} strokeWidth="5" strokeLinecap="round" strokeLinejoin="round" />
  </svg>
);

/** Night backdrop with a slow, faint signal-blue glow. */
export const Backdrop: React.FC<{tint?: string; loop?: number}> = ({tint = 'rgba(58,123,213,0.10)', loop}) => {
  const f = useCurrentFrame();
  // With `loop`, the drift completes whole cycles so the last frame meets the first.
  const a = loop ? (f / loop) * Math.PI * 2 : f / 90;
  const b = loop ? (f / loop) * Math.PI * 2 : f / 110;
  const x = 30 + Math.sin(a) * 8;
  const y = 20 + Math.cos(b) * 6;
  return (
    <AbsoluteFill
      style={{
        backgroundColor: C.night,
        backgroundImage: `radial-gradient(1200px 900px at ${x}% ${y}%, ${tint}, rgba(18,20,24,0) 70%)`,
      }}
    />
  );
};

/** Fades a scene in and out at its edges so cuts never pop. */
export const SceneFade: React.FC<{dur: number; inF?: number; outF?: number; children: React.ReactNode}> = ({
  dur,
  inF = 8,
  outF = 8,
  children,
}) => {
  const f = useCurrentFrame();
  const a = inF > 0 ? interpolate(f, [0, inF], [0, 1], {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'}) : 1;
  const b = outF > 0 ? interpolate(f, [dur - outF, dur], [1, 0], {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'}) : 1;
  return <AbsoluteFill style={{opacity: Math.min(a, b)}}>{children}</AbsoluteFill>;
};

export const Kicker: React.FC<{text: string; color: string; style?: React.CSSProperties}> = ({text, color, style}) => (
  <div
    style={{
      display: 'inline-flex',
      alignItems: 'center',
      gap: 14,
      fontFamily: F.body,
      fontWeight: 600,
      fontSize: 26,
      letterSpacing: '0.16em',
      textTransform: 'uppercase',
      color,
      ...style,
    }}
  >
    <span style={{width: 14, height: 14, borderRadius: 7, background: color, display: 'inline-block'}} />
    {text}
  </div>
);
