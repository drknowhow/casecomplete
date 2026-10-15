import React from 'react';
import {AbsoluteFill, spring, useCurrentFrame, useVideoConfig} from 'remotion';
import {C, F, type Layout} from '../theme';
import {Mark, Wordmark} from '../components/brand';

export const END_DUR = 150;

export const End: React.FC<{layout: Layout}> = ({layout}) => {
  const frame = useCurrentFrame();
  const {fps} = useVideoConfig();
  const portrait = layout === 'portrait';
  const mark = spring({frame, fps, config: {damping: 13, mass: 0.8}});
  const word = spring({frame: frame - 8, fps, config: {damping: 200}, durationInFrames: 20});
  const tag = spring({frame: frame - 22, fps, config: {damping: 200}, durationInFrames: 18});
  const meta = spring({frame: frame - 36, fps, config: {damping: 200}, durationInFrames: 18});
  const url = spring({frame: frame - 48, fps, config: {damping: 200}, durationInFrames: 18});

  return (
    <AbsoluteFill style={{alignItems: 'center', justifyContent: 'center'}}>
      <div style={{display: 'flex', flexDirection: 'column', alignItems: 'center', gap: portrait ? 44 : 40}}>
        <div
          style={{
            display: 'flex',
            flexDirection: portrait ? 'column' : 'row',
            alignItems: 'center',
            gap: portrait ? 40 : 44,
          }}
        >
          <div style={{transform: `scale(${mark}) rotate(${(1 - mark) * -20}deg)`}}>
            <Mark size={portrait ? 240 : 190} />
          </div>
          <div
            style={{
              opacity: word,
              transform: portrait ? `translateY(${(1 - word) * 30}px)` : `translateX(${(1 - word) * -40}px)`,
            }}
          >
            <Wordmark size={portrait ? 120 : 150} />
          </div>
        </div>
        <div
          style={{
            fontFamily: F.display,
            fontWeight: 600,
            fontSize: portrait ? 84 : 80,
            color: C.paper,
            letterSpacing: '-0.02em',
            opacity: tag,
            transform: `translateY(${(1 - tag) * 24}px)`,
            marginTop: 10,
          }}
        >
          Just type<span style={{color: C.blue}}>.</span>
        </div>
        <div
          style={{
            fontFamily: F.body,
            fontSize: portrait ? 40 : 38,
            color: C.dim,
            opacity: meta,
            transform: `translateY(${(1 - meta) * 20}px)`,
          }}
        >
          Free, open source, Windows.
        </div>
        <div
          style={{
            fontFamily: F.mono,
            fontSize: portrait ? 34 : 34,
            color: C.paper,
            background: C.tile,
            border: `2px solid ${C.hairStrong}`,
            borderRadius: 16,
            padding: '16px 30px',
            opacity: url,
            transform: `translateY(${(1 - url) * 20}px)`,
          }}
        >
          github.com/drknowhow/casecomplete
        </div>
      </div>
    </AbsoluteFill>
  );
};
