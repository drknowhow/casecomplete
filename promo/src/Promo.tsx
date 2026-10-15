import React from 'react';
import {AbsoluteFill, Series} from 'remotion';
import type {Layout} from './theme';
import {Backdrop, SceneFade} from './components/brand';
import {HOOK_DUR, Hook} from './scenes/Hook';
import {SWITCH_DUR, Switch} from './scenes/Switch';
import {WITH_DUR, With} from './scenes/With';
import {LEARNS_DUR, Learns} from './scenes/Learns';
import {PRIVATE_DUR, Private} from './scenes/Private';
import {END_DUR, End} from './scenes/End';

export type PromoProps = {layout: Layout};

const SCENES: {id: string; C: React.FC<{layout: Layout}>; dur: number; inF?: number; outF?: number}[] = [
  {id: 'hook', C: Hook, dur: HOOK_DUR, inF: 0},
  {id: 'switch', C: Switch, dur: SWITCH_DUR, inF: 4},
  {id: 'with', C: With, dur: WITH_DUR},
  {id: 'learns', C: Learns, dur: LEARNS_DUR},
  {id: 'private', C: Private, dur: PRIVATE_DUR},
  {id: 'end', C: End, dur: END_DUR, outF: 0},
];

export const PROMO_DUR = SCENES.reduce((a, s) => a + s.dur, 0);

export const Promo: React.FC<PromoProps> = ({layout}) => (
  <AbsoluteFill>
    <Backdrop />
    <Series>
      {SCENES.map((s) => (
        <Series.Sequence key={s.id} name={s.id} durationInFrames={s.dur}>
          <SceneFade dur={s.dur} inF={s.inF} outF={s.outF}>
            <s.C layout={layout} />
          </SceneFade>
        </Series.Sequence>
      ))}
    </Series>
  </AbsoluteFill>
);
