import React from 'react';
import {AbsoluteFill, interpolate, spring, useCurrentFrame, useVideoConfig} from 'remotion';
import {C, F, type Layout} from '../theme';
import {Mark} from '../components/brand';

const clamp = {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'} as const;

export const SWITCH_DUR = 66;
const LAND = 14;

export const Switch: React.FC<{layout: Layout}> = ({layout}) => {
  const frame = useCurrentFrame();
  const {fps} = useVideoConfig();
  const portrait = layout === 'portrait';
  const size = portrait ? 260 : 280;

  // Drop in, bottom out like a pressed key, settle.
  const drop = spring({frame, fps, config: {damping: 14, stiffness: 140, mass: 0.9}, durationInFrames: LAND + 6});
  const y = interpolate(drop, [0, 1], [-620, 0]);
  const rot = interpolate(drop, [0, 1], [-14, 0]);
  const squash = interpolate(frame, [LAND - 2, LAND + 1, LAND + 5, LAND + 12], [0, 1, 0.35, 0], clamp);
  const ring = interpolate(frame, [LAND, LAND + 28], [0, 1], clamp);

  const words = ['Just', 'type.'];
  return (
    <AbsoluteFill style={{alignItems: 'center', justifyContent: 'center'}}>
      <div style={{display: 'flex', flexDirection: 'column', alignItems: 'center', gap: portrait ? 70 : 56}}>
        <div style={{position: 'relative', width: size, height: size}}>
          <div
            style={{
              position: 'absolute',
              left: '50%',
              top: '50%',
              width: size * (1.15 + ring * 1.9),
              height: size * (1.15 + ring * 1.9),
              transform: 'translate(-50%, -50%)',
              borderRadius: '50%',
              border: `${10 * (1 - ring) + 2}px solid ${C.blue}`,
              opacity: ring > 0 ? (1 - ring) * 0.9 : 0,
            }}
          />
          <div
            style={{
              transform: `translateY(${y + squash * 10}px) rotate(${rot}deg) scale(${1 + squash * 0.05}, ${1 - squash * 0.08})`,
              transformOrigin: '50% 100%',
              filter: `drop-shadow(0 ${30 - squash * 18}px 50px rgba(58,123,213,${0.25 + squash * 0.3}))`,
            }}
          >
            <Mark size={size} />
          </div>
        </div>
        <div
          style={{
            display: 'flex',
            gap: '0.28em',
            fontFamily: F.display,
            fontWeight: 600,
            fontSize: portrait ? 132 : 150,
            letterSpacing: '-0.03em',
            color: C.paper,
            lineHeight: 1,
          }}
        >
          {words.map((w, i) => {
            const s = spring({frame: frame - 20 - i * 6, fps, config: {damping: 18, mass: 0.6}});
            return (
              <span key={w} style={{opacity: s, transform: `translateY(${(1 - s) * 40}px)`, display: 'inline-block'}}>
                {w}
              </span>
            );
          })}
        </div>
      </div>
    </AbsoluteFill>
  );
};
