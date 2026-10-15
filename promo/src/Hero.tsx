import React from 'react';
import {AbsoluteFill, interpolate, useCurrentFrame} from 'remotion';
import {C, F} from './theme';
import {typeWords, type Part} from './lib/typing';
import {Backdrop, Mark, Wordmark} from './components/brand';
import {TabKey, pressDepth} from './components/ui';
import {Email} from './scenes/With';

const clamp = {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'} as const;

// A 6-second seamless loop of the "with" moment for the README.
export const HERO_DUR = 180;
const T0 = 14;
const s = typeWords(T0, 'I hahve>have been stuck on thsi>this becuase>because teh>the', 'hero', 2.1);
const G = {text: 'parser drops empty lines.', showT: s.end + 6, acceptT: s.end + 34};
const PARTS: Part[] = [s.part, {kind: 'ghost', ghost: G}];
const FADE = 150;
const RESET = 166;

export const Hero: React.FC = () => {
  const frame = useCurrentFrame();
  // After the fade the composer is empty again, matching frame 0 so the loop is seamless.
  const f = frame >= RESET ? frame - HERO_DUR : frame;
  const docOpacity = frame >= RESET ? 1 : interpolate(frame, [FADE, RESET - 2], [1, 0], clamp);
  const depth = pressDepth(frame, [G.acceptT]);
  // The keycap stays on screen (the loop never pops); its caption lights up while there is a ghost to take.
  const capO = Math.min(
    interpolate(frame, [G.showT - 4, G.showT + 4], [0.35, 1], clamp),
    interpolate(frame, [FADE, RESET - 2], [1, 0.35], clamp),
  );

  return (
    <AbsoluteFill>
      <Backdrop loop={HERO_DUR} />
      <AbsoluteFill style={{padding: '70px 110px', display: 'flex', flexDirection: 'column', gap: 40}}>
        <div style={{display: 'flex', alignItems: 'center', gap: 22}}>
          <Mark size={78} />
          <Wordmark size={58} />
          <div style={{fontFamily: F.display, fontWeight: 500, fontSize: 40, color: C.ghost, marginLeft: 18}}>
            Just type.
          </div>
        </div>
        <Email width={1700} fontSize={64} frame={f} lines={3.5} parts={PARTS} start={0} docOpacity={docOpacity} />
        <div style={{display: 'flex', alignItems: 'center', justifyContent: 'flex-end', gap: 36}}>
          <div style={{fontFamily: F.body, fontWeight: 500, fontSize: 44, color: C.paper, opacity: capO}}>
            Tab accepts the sentence
          </div>
          <TabKey depth={depth} scale={1.1} />
        </div>
      </AbsoluteFill>
    </AbsoluteFill>
  );
};
